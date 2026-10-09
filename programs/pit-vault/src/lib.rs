//! Agent Pit vault.
//!
//! Every agent on the floor trades from a wallet that is a program address:
//! a hash that is not a point on the curve, so no private key for it exists
//! or can exist. This program is the only thing that can sign for it, and it
//! will only ever do three things with that signature.
//!
//!   1. Swap between SOL and one coin through the configured router, when the
//!      floor's trading key asks. The router is handed the vault's signature
//!      for the length of one call and is not trusted with it: afterwards the
//!      program checks, on the vault's own accounts, that only what was meant
//!      to move has moved and that nothing was quietly given a new owner or a delegate.
//!   2. Pay the agent's operator, and only the operator, when they withdraw.
//!   3. Take the clearing fee on a fill.
//!
//! What the trading key cannot do is send funds anywhere. What it still can
//! do, if stolen, is trade badly: it chooses the route and the minimum it
//! will accept. The buy cap, the operator's halt and the admin's pause bound
//! that damage. They do not remove it.
//!
//! Every agent also has a bank. The program keeps the agent's realised profit
//! to the lamport, and whenever a sale takes that profit to a new high, the
//! new profit leaves the trading wallet for the bank, less a small cut of it.
//! That cut is the only fee on gains: it is taken on gains above the agent's
//! high water mark and nothing else, and all of it goes to buy $PIT and burn
//! it. An operator who holds $PIT pays half of it, and half the clearing fee. Once a day a fixed share of what was newly banked is sent on
//! to the operator's own wallet. Only profit ever moves this way: the money
//! the operator put in stays in the wallet, and nothing is locked. The
//! operator can take their capital, and the rest of the bank, whenever they like.
//!
//! This version holds the operator's own money only. Other people's deposits
//! need share accounting and a way to price open positions, and are not in here.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    bpf_loader_upgradeable,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
};
use anchor_lang::system_program::{self, Allocate, Assign, CreateAccount, Transfer};

pub mod token;
use token::{burn_checked, close_account, decimals, initialize_account3, is_token_program, must_view, transfer_checked, view, ACCOUNT_LEN, NATIVE_MINT, TOKEN};

declare_id!("6PNL53h67LUSn252YL96VY51N6hX5cz1i6Rh3pt9Pmjf");

const BPS: u128 = 10_000;
/// Ceilings the admin cannot configure past.
pub const MAX_CLEARING_FEE_BPS: u16 = 100;
pub const MAX_PAYOUT_BPS: u16 = 2_000;
pub const MAX_PROFIT_FEE_BPS: u16 = 500;
/// Profit is shared with the operator at most once in this many seconds.
pub const PAYOUT_PERIOD: i64 = 86_400;
/// New terms wait this long before they can take effect. Whoever holds the
/// admin key cannot swap the router or the trading key under an operator
/// overnight: the change is announced on chain a day ahead, and anyone who
/// does not like it has that day to take their money out.
pub const TERMS_DELAY: i64 = 86_400;
pub const MAX_BUY_BPS: u16 = 5_000;
/// Fees are swept once there is enough to be worth three transfers.
pub const MIN_COLLECT: u64 = 50_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Buy,
    Sell,
}

#[program]
pub mod pit_vault {
    use super::*;

    /// One time setup, by whoever holds the program's upgrade authority and
    /// nobody else, so the deployment cannot be claimed by a stranger.
    pub fn initialize(ctx: Context<Initialize>, terms: Terms) -> Result<()> {
        let admin = ctx.accounts.admin.key();
        terms.check(&admin)?;
        let config = &mut ctx.accounts.config;
        config.admin = admin;
        config.pending_admin = Pubkey::default();
        config.pending = terms.clone();
        config.pending_at = 0;
        config.has_pending = false;
        config.terms = terms;
        config.paused = false;
        config.agents = 0;
        config.bump = ctx.bumps.config;
        config.fees_bump = ctx.bumps.fees;
        config.gains_bump = ctx.bumps.gains_pot;
        // The two fee accounts have to hold their rent before they can take a small fee.
        let floor = Rent::get()?.minimum_balance(0);
        for pot in [ctx.accounts.fees.to_account_info(), ctx.accounts.gains_pot.to_account_info()] {
            let short = floor.saturating_sub(pot.lamports());
            if short > 0 {
                system_program::transfer(CpiContext::new(ctx.accounts.system_program.to_account_info(), Transfer { from: ctx.accounts.admin.to_account_info(), to: pot }), short)?;
            }
        }
        Ok(())
    }

    /// Announces new terms. Nothing changes yet.
    pub fn propose_terms(ctx: Context<AdminOnly>, terms: Terms) -> Result<()> {
        terms.check(&ctx.accounts.admin.key())?;
        let now = Clock::get()?.unix_timestamp;
        let config = &mut ctx.accounts.config;
        config.pending = terms;
        config.pending_at = now;
        config.has_pending = true;
        emit!(TermsProposed { at: now, applies_at: now + TERMS_DELAY });
        Ok(())
    }

    /// Puts announced terms into effect, once they have waited out the delay.
    pub fn apply_terms(ctx: Context<AdminOnly>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let admin = ctx.accounts.admin.key();
        let config = &mut ctx.accounts.config;
        require!(config.has_pending, VaultError::NoTermsPending);
        require!(now - config.pending_at >= TERMS_DELAY, VaultError::TermsNotReady);
        // The admin may have changed hands while these waited.
        config.pending.check(&admin)?;
        config.terms = config.pending.clone();
        config.has_pending = false;
        Ok(())
    }

    pub fn cancel_terms(ctx: Context<AdminOnly>) -> Result<()> {
        ctx.accounts.config.has_pending = false;
        Ok(())
    }

    /// Stops every swap on the floor. Withdrawals are never stopped.
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    pub fn propose_admin(ctx: Context<AdminOnly>, next: Pubkey) -> Result<()> {
        ctx.accounts.config.pending_admin = next;
        Ok(())
    }

    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        let next = ctx.accounts.next.key();
        require!(next != config.terms.executor && next != config.terms.registrar, VaultError::RolesNotSeparate);
        config.admin = next;
        config.pending_admin = Pubkey::default();
        Ok(())
    }

    /// Burn $PIT, get an agent. The registrar signs too: it is the floor's
    /// record of which number belongs to which agent, and the number is what
    /// the wallet address is derived from.
    ///
    /// A spawn costs a fixed sum in SOL, so the number of $PIT it burns moves
    /// with the price. This program cannot see a price. The registrar sets
    /// `burn` for each spawn, the operator sees it in their wallet before
    /// signing, and the terms hold the least it may ever be.
    ///
    /// `exit_fee_only` is the agent's fee class, set once here by the registrar
    /// and never changed: an agent that trades in and out all day pays the
    /// clearing fee on the way out only.
    pub fn spawn(ctx: Context<Spawn>, id: u64, burn: u64, exit_fee_only: bool) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, VaultError::Paused);
        require!(config.terms.pit_mint != Pubkey::default() && config.terms.spawn_burn_min > 0, VaultError::SpawnClosed);
        require!(burn >= config.terms.spawn_burn_min, VaultError::BurnTooSmall);

        let mint = ctx.accounts.pit_mint.to_account_info();
        let program = ctx.accounts.token_program.to_account_info();
        require!(is_token_program(program.key) && mint.owner == program.key, VaultError::WrongTokenProgram);
        let burned = burn;
        invoke(
            &burn_checked(program.key, ctx.accounts.operator_pit.key, mint.key, ctx.accounts.operator.key, burned, decimals(&mint)?),
            &[ctx.accounts.operator_pit.to_account_info(), mint, ctx.accounts.operator.to_account_info()],
        )?;

        ctx.accounts.agent.set_inner(Agent::new(id, ctx.accounts.operator.key(), exit_fee_only, ctx.bumps.agent, ctx.bumps.vault, ctx.bumps.wsol, ctx.bumps.bank)?);
        ctx.accounts.config.agents += 1;
        emit!(Spawned { id, operator: ctx.accounts.operator.key(), vault: ctx.accounts.vault.key(), burned });
        Ok(())
    }

    /// Registers an agent without a burn. For the house agents and for agents
    /// carried over from the paper season. Admin only.
    pub fn adopt(ctx: Context<Adopt>, id: u64, operator: Pubkey, exit_fee_only: bool) -> Result<()> {
        ctx.accounts.agent.set_inner(Agent::new(id, operator, exit_fee_only, ctx.bumps.agent, ctx.bumps.vault, ctx.bumps.wsol, ctx.bumps.bank)?);
        ctx.accounts.config.agents += 1;
        emit!(Spawned { id, operator, vault: ctx.accounts.vault.key(), burned: 0 });
        Ok(())
    }

    /// Holders of $PIT pay half: half the clearing fee on every fill, and half
    /// the cut of gains. An agent is put in the holder class when its
    /// operator's own wallet holds enough $PIT, and taken out when it no
    /// longer does.
    ///
    /// How much is enough is a fixed sum in SOL, so the number of $PIT moves
    /// with the price, and this program cannot see a price. As with a spawn,
    /// the registrar says what the sum comes to today, and the terms hold the
    /// least it may ever be. The program checks the rest itself: that the
    /// account is $PIT, that it belongs to this agent's operator, and that it
    /// holds that much. The worst a careless registrar can do here is charge
    /// someone half who should have paid in full.
    pub fn grant_holder(ctx: Context<GrantHolder>, min: u64) -> Result<()> {
        let terms = &ctx.accounts.config.terms;
        require!(terms.pit_mint != Pubkey::default() && terms.holder_min > 0, VaultError::HolderClosed);
        require!(min >= terms.holder_min, VaultError::HoldingTooSmall);
        let held = must_view(&ctx.accounts.operator_pit.to_account_info())?;
        require!(held.mint == terms.pit_mint, VaultError::WrongMint);
        require!(held.owner == ctx.accounts.agent.operator, VaultError::NotOperatorAccount);
        require!(held.amount >= min, VaultError::HoldingTooSmall);
        ctx.accounts.agent.holder = true;
        emit!(HolderSet { id: ctx.accounts.agent.id, holder: true });
        Ok(())
    }

    /// Back to full fees, when the operator no longer holds enough.
    pub fn revoke_holder(ctx: Context<RevokeHolder>) -> Result<()> {
        ctx.accounts.agent.holder = false;
        emit!(HolderSet { id: ctx.accounts.agent.id, holder: false });
        Ok(())
    }

    /// The operator's brake. A halted agent can sell what it holds and cannot
    /// buy anything new.
    pub fn set_halted(ctx: Context<OperatorOnly>, halted: bool) -> Result<()> {
        ctx.accounts.agent.halted = halted;
        Ok(())
    }

    /// SOL out of the vault, to the operator and nowhere else. Works whether
    /// or not the floor is paused. Pass u64::MAX to take everything.
    pub fn withdraw(ctx: Context<Withdraw>, lamports: u64) -> Result<()> {
        let vault = ctx.accounts.vault.to_account_info();
        let have = vault.lamports();
        let lamports = if lamports == u64::MAX { have } else { lamports };
        require!(lamports > 0 && lamports <= have, VaultError::InsufficientFunds);
        // A wallet is either empty or holds its rent. Nothing in between.
        let left = have - lamports;
        require!(left == 0 || left >= Rent::get()?.minimum_balance(0), VaultError::BelowRent);

        let id = ctx.accounts.agent.id.to_le_bytes();
        let seeds: &[&[u8]] = &[b"vault", &id, &[ctx.accounts.agent.vault_bump]];
        system_program::transfer(
            CpiContext::new_with_signer(ctx.accounts.system_program.to_account_info(), Transfer { from: vault, to: ctx.accounts.operator.to_account_info() }, &[seeds]),
            lamports,
        )?;
        emit!(Withdrawn { id: ctx.accounts.agent.id, lamports });
        Ok(())
    }

    /// A coin out of the vault as it is, to a token account the operator owns.
    /// The way out that needs nobody else: not the trading key, not the floor.
    pub fn withdraw_token<'info>(ctx: Context<'_, '_, '_, 'info, WithdrawToken<'info>>, amount: u64) -> Result<()> {
        let vault = ctx.accounts.vault.to_account_info();
        let from = ctx.accounts.from.to_account_info();
        let to = ctx.accounts.to.to_account_info();
        let mint = ctx.accounts.mint.to_account_info();
        let program = ctx.accounts.token_program.to_account_info();

        let source = must_view(&from)?;
        let dest = must_view(&to)?;
        require_keys_eq!(source.owner, vault.key(), VaultError::NotVaultAccount);
        require_keys_eq!(dest.owner, ctx.accounts.operator.key(), VaultError::NotOperatorAccount);
        require!(source.mint == mint.key() && dest.mint == mint.key(), VaultError::WrongMint);
        require!(from.owner == program.key && is_token_program(program.key), VaultError::WrongTokenProgram);
        require!(amount > 0 && amount <= source.amount, VaultError::InsufficientFunds);

        let mut ix = transfer_checked(program.key, from.key, mint.key, to.key, vault.key, amount, decimals(&mint)?);
        let mut infos = vec![from, mint, to, vault];
        // Whatever a transfer hook on the mint needs rides along.
        for ai in ctx.remaining_accounts {
            ix.accounts.push(AccountMeta { pubkey: ai.key(), is_signer: false, is_writable: ai.is_writable });
            infos.push(ai.clone());
        }
        let id = ctx.accounts.agent.id.to_le_bytes();
        invoke_signed(&ix, &infos, &[&[b"vault", &id, &[ctx.accounts.agent.vault_bump]]])?;
        // Coins taken out as they are leave with their share of what they
        // cost. It is capital leaving, not a profit and not a loss.
        if let Some(position) = ctx.accounts.position.as_mut() {
            position.cost -= portion(position.cost, amount, source.amount);
        }
        Ok(())
    }

    /// The operator's share of what the agent has banked since the last time,
    /// sent from the bank to the operator's own wallet. Anyone can turn this
    /// crank once a day. Only banked profit is ever shared, so the money the
    /// operator put in is never part of it, and after a loss nothing reaches
    /// the bank, and so nothing is paid, until the agent has made the loss back.
    pub fn payout(ctx: Context<Payout>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let bps = ctx.accounts.config.terms.payout_bps;
        let agent = &ctx.accounts.agent;
        require!(now - agent.last_payout >= PAYOUT_PERIOD, VaultError::TooSoon);
        require!(agent.unshared > 0 && bps > 0, VaultError::NothingToPay);
        let due = share(agent.unshared, bps);
        let bank = ctx.accounts.bank.to_account_info();
        let lamports = due.min(bank.lamports().saturating_sub(Rent::get()?.minimum_balance(0)));
        require!(lamports > 0, VaultError::NothingToPay);
        // If the bank is short, pay what is there and leave the rest owing.
        let covered = if lamports == due { agent.unshared } else { (lamports as u128 * BPS / bps as u128) as u64 };

        let id = agent.id.to_le_bytes();
        system_program::transfer(
            CpiContext::new_with_signer(ctx.accounts.system_program.to_account_info(), Transfer { from: bank, to: ctx.accounts.operator.to_account_info() }, &[&[b"bank", &id, &[agent.bank_bump]]]),
            lamports,
        )?;
        let agent = &mut ctx.accounts.agent;
        agent.unshared -= covered;
        agent.paid_out += lamports;
        agent.last_payout = now;
        emit!(PaidOut { id: agent.id, operator: agent.operator, lamports });
        Ok(())
    }

    /// The rest of the bank is the operator's too, and comes out whenever
    /// they like. All it holds back is the share already due at the next
    /// payout, which is going to the same wallet anyway. u64::MAX takes all
    /// that can be taken.
    pub fn withdraw_bank(ctx: Context<WithdrawBank>, lamports: u64) -> Result<()> {
        let bank = ctx.accounts.bank.to_account_info();
        let agent = &ctx.accounts.agent;
        let floor = Rent::get()?.minimum_balance(0);
        let held_back = share(agent.unshared, ctx.accounts.config.terms.payout_bps);
        let free = bank.lamports().saturating_sub(floor).saturating_sub(held_back);
        let lamports = if lamports == u64::MAX { free } else { lamports };
        require!(lamports > 0 && lamports <= free, VaultError::BankHoldsNextPayout);
        let id = agent.id.to_le_bytes();
        system_program::transfer(
            CpiContext::new_with_signer(ctx.accounts.system_program.to_account_info(), Transfer { from: bank, to: ctx.accounts.operator.to_account_info() }, &[&[b"bank", &id, &[agent.bank_bump]]]),
            lamports,
        )?;
        emit!(Withdrawn { id: agent.id, lamports });
        Ok(())
    }

    /// SOL into one coin. `data` and the remaining accounts are the router's
    /// own instruction, passed through as given.
    pub fn buy<'info>(ctx: Context<'_, '_, '_, 'info, Swap<'info>>, amount_in: u64, min_out: u64, data: Vec<u8>) -> Result<()> {
        trade(ctx, Side::Buy, amount_in, min_out, data)
    }

    /// One coin back into SOL.
    pub fn sell<'info>(ctx: Context<'_, '_, '_, 'info, Swap<'info>>, amount_in: u64, min_out: u64, data: Vec<u8>) -> Result<()> {
        trade(ctx, Side::Sell, amount_in, min_out, data)
    }

    /// Closes an empty token account the vault no longer needs and returns its
    /// rent to whoever asked: the trading key, which paid to open it, or the
    /// operator.
    pub fn close_token(ctx: Context<CloseToken>) -> Result<()> {
        let who = ctx.accounts.authority.key();
        require!(who == ctx.accounts.config.terms.executor || who == ctx.accounts.agent.operator, VaultError::NotAllowed);
        let account = ctx.accounts.account.to_account_info();
        let vault = ctx.accounts.vault.to_account_info();
        let program = ctx.accounts.token_program.to_account_info();
        let seen = must_view(&account)?;
        require_keys_eq!(seen.owner, vault.key(), VaultError::NotVaultAccount);
        require!(seen.amount == 0, VaultError::NotEmpty);
        require!(account.owner == program.key, VaultError::WrongTokenProgram);
        let id = ctx.accounts.agent.id.to_le_bytes();
        invoke_signed(
            &close_account(program.key, account.key, &who, vault.key),
            &[account, ctx.accounts.authority.to_account_info(), vault],
            &[&[b"vault", &id, &[ctx.accounts.agent.vault_bump]]],
        )?;
        Ok(())
    }

    /// Sends collected fees where they go. Clearing fees split three ways.
    /// The cut of gains goes whole to the burner, to buy $PIT and burn it.
    /// Anyone can turn this crank: the destinations and the split are fixed
    /// in the config.
    pub fn collect_fees(ctx: Context<CollectFees>) -> Result<()> {
        let fees = ctx.accounts.fees.to_account_info();
        let pot = ctx.accounts.gains_pot.to_account_info();
        let terms = &ctx.accounts.config.terms;
        let floor = Rent::get()?.minimum_balance(0);
        let clearing = fees.lamports().saturating_sub(floor);
        let profit = pot.lamports().saturating_sub(floor);
        require!(clearing + profit >= MIN_COLLECT, VaultError::NothingToCollect);
        let to_treasury = share(clearing, terms.treasury_bps);
        let to_burn = share(clearing, terms.burn_bps);
        let to_ecosystem = clearing - to_treasury - to_burn;
        let system = ctx.accounts.system_program.to_account_info();
        let fee_seeds: &[&[u8]] = &[b"fees", &[ctx.accounts.config.fees_bump]];
        for (to, lamports) in [(&ctx.accounts.treasury, to_treasury), (&ctx.accounts.burner, to_burn), (&ctx.accounts.ecosystem, to_ecosystem)] {
            if lamports > 0 {
                system_program::transfer(CpiContext::new_with_signer(system.clone(), Transfer { from: fees.clone(), to: to.to_account_info() }, &[fee_seeds]), lamports)?;
            }
        }
        if profit > 0 {
            let pot_seeds: &[&[u8]] = &[b"gains", &[ctx.accounts.config.gains_bump]];
            system_program::transfer(CpiContext::new_with_signer(system, Transfer { from: pot, to: ctx.accounts.burner.to_account_info() }, &[pot_seeds]), profit)?;
        }
        emit!(FeesCollected { treasury: to_treasury, burn: to_burn + profit, ecosystem: to_ecosystem });
        Ok(())
    }
}

fn share(amount: u64, bps: u16) -> u64 {
    (amount as u128 * bps as u128 / BPS) as u64
}

/// The part of `whole` that goes with `part` out of `of`. All of it when
/// nothing is left behind, so no dust of cost outlives its coins.
fn portion(whole: u64, part: u64, of: u64) -> u64 {
    if part >= of || of == 0 {
        whole
    } else {
        (whole as u128 * part as u128 / of as u128) as u64
    }
}

/// One fill. The same path for both directions, because the same things have
/// to be true afterwards whichever way the money went.
fn trade<'info>(ctx: Context<'_, '_, '_, 'info, Swap<'info>>, side: Side, amount_in: u64, min_out: u64, data: Vec<u8>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(!config.paused, VaultError::Paused);
    require!(amount_in > 0, VaultError::ZeroAmount);
    require!(side == Side::Sell || !ctx.accounts.agent.halted, VaultError::Halted);

    let id = ctx.accounts.agent.id.to_le_bytes();
    let vault_seeds: &[&[u8]] = &[b"vault", &id, &[ctx.accounts.agent.vault_bump]];
    let wsol_seeds: &[&[u8]] = &[b"wsol", &id, &[ctx.accounts.agent.wsol_bump]];

    let vault = ctx.accounts.vault.to_account_info();
    let wsol = ctx.accounts.wsol.to_account_info();
    let coin = ctx.accounts.coin.to_account_info();
    let system = ctx.accounts.system_program.to_account_info();

    // The coin side must be the vault's own account, for a coin, untouched.
    let before = must_view(&coin)?;
    require_keys_eq!(before.owner, vault.key(), VaultError::NotVaultAccount);
    require!(before.mint != NATIVE_MINT, VaultError::NotACoin);
    require_keys_eq!(before.mint, ctx.accounts.coin_mint.key(), VaultError::WrongMint);
    require!(!before.has_delegate && !before.has_close_authority, VaultError::AccountTampered);

    let floor = Rent::get()?.minimum_balance(0);
    let vault_before = vault.lamports();
    // Anyone can park lamports on the wrapped SOL address. They are swept into
    // the vault with everything else, and counted so the sums still close.
    let parked = wsol.lamports();

    match side {
        Side::Buy => {
            let cap = share(vault_before.saturating_sub(floor), config.terms.max_buy_bps);
            require!(amount_in <= cap, VaultError::ClipTooLarge);
        }
        Side::Sell => require!(amount_in <= before.amount, VaultError::InsufficientFunds),
    }

    // Wrapped SOL lives only for the length of this instruction: opened here
    // out of the vault, holding what a buy may spend, closed back into the
    // vault below. The vault's balance is always plain SOL at rest.
    require!(*wsol.owner == system.key() && wsol.data_is_empty(), VaultError::WrappedBusy);
    let want = Rent::get()?.minimum_balance(ACCOUNT_LEN).checked_add(if side == Side::Buy { amount_in } else { 0 }).ok_or(VaultError::Overflow)?;
    if parked == 0 {
        system_program::create_account(CpiContext::new_with_signer(system.clone(), CreateAccount { from: vault.clone(), to: wsol.clone() }, &[vault_seeds, wsol_seeds]), want, ACCOUNT_LEN as u64, &TOKEN)?;
    } else {
        if want > parked {
            system_program::transfer(CpiContext::new_with_signer(system.clone(), Transfer { from: vault.clone(), to: wsol.clone() }, &[vault_seeds]), want - parked)?;
        }
        system_program::allocate(CpiContext::new_with_signer(system.clone(), Allocate { account_to_allocate: wsol.clone() }, &[wsol_seeds]), ACCOUNT_LEN as u64)?;
        system_program::assign(CpiContext::new_with_signer(system.clone(), Assign { account_to_assign: wsol.clone() }, &[wsol_seeds]), &TOKEN)?;
    }
    invoke(&initialize_account3(&TOKEN, wsol.key, &NATIVE_MINT, vault.key), &[wsol.clone(), ctx.accounts.native_mint.to_account_info()])?;
    let wrapped_before = must_view(&wsol)?.amount;

    // The router gets the vault's signature, so it may be shown exactly two of
    // the vault's token accounts: this coin and the wrapped SOL. Any other
    // holding stays out of its reach.
    for ai in ctx.remaining_accounts {
        if ai.key() == wsol.key() || ai.key() == coin.key() {
            continue;
        }
        if let Some(seen) = view(ai) {
            require!(seen.owner != vault.key(), VaultError::ForeignVaultAccount);
        }
    }

    let metas = ctx.remaining_accounts.iter().map(|ai| AccountMeta { pubkey: ai.key(), is_signer: ai.key() == vault.key(), is_writable: ai.is_writable }).collect();
    let mut infos = ctx.remaining_accounts.to_vec();
    infos.push(ctx.accounts.router.to_account_info());
    invoke_signed(&Instruction { program_id: ctx.accounts.router.key(), accounts: metas, data }, &infos, &[vault_seeds])?;

    // The router has had the signature. Now check what it did with it.
    require!(*vault.owner == system.key() && vault.data_is_empty(), VaultError::VaultTampered);
    let after = must_view(&coin)?;
    let wrapped = must_view(&wsol)?;
    require!(after.owner == vault.key() && after.mint == before.mint && !after.has_delegate && !after.has_close_authority, VaultError::AccountTampered);
    require!(wrapped.owner == vault.key() && wrapped.mint == NATIVE_MINT && !wrapped.has_delegate && !wrapped.has_close_authority, VaultError::AccountTampered);

    let (sol, tokens) = match side {
        Side::Buy => {
            let spent = wrapped_before.checked_sub(wrapped.amount).ok_or(VaultError::AccountTampered)?;
            let got = after.amount.checked_sub(before.amount).ok_or(VaultError::AccountTampered)?;
            require!(spent <= amount_in, VaultError::TookTooMuch);
            require!(got > 0 && got >= min_out, VaultError::BelowMinimum);
            (spent, got)
        }
        Side::Sell => {
            let sold = before.amount.checked_sub(after.amount).ok_or(VaultError::AccountTampered)?;
            let proceeds = wrapped.amount.checked_sub(wrapped_before).ok_or(VaultError::AccountTampered)?;
            require!(sold <= amount_in, VaultError::TookTooMuch);
            require!(proceeds > 0 && proceeds >= min_out, VaultError::BelowMinimum);
            (proceeds, sold)
        }
    };

    // Unwrap: everything in the wrapped account, rent included, back to the vault.
    invoke_signed(&close_account(&TOKEN, wsol.key, vault.key, vault.key), &[wsol.clone(), vault.clone(), vault.clone()], &[vault_seeds])?;

    // An agent in the exit only fee class pays nothing to get in. One whose
    // operator holds $PIT pays half of whatever it would have paid.
    let holder = ctx.accounts.agent.holder;
    let half = |fee: u64| if holder { fee / 2 } else { fee };
    let fee = if matches!(side, Side::Buy) && ctx.accounts.agent.exit_fee_only { 0 } else { half(share(sol, config.terms.clearing_fee_bps)) };
    if fee > 0 {
        system_program::transfer(CpiContext::new_with_signer(system.clone(), Transfer { from: vault.clone(), to: ctx.accounts.fees.to_account_info() }, &[vault_seeds]), fee)?;
    }

    // The books have to close to the lamport. A buy leaves the vault down by
    // what was spent and the fee, a sell up by the proceeds less the fee.
    let base = vault_before.checked_add(parked).ok_or(VaultError::Overflow)?;
    let expected = match side {
        Side::Buy => base.checked_sub(sol).and_then(|v| v.checked_sub(fee)),
        Side::Sell => base.checked_add(sol).and_then(|v| v.checked_sub(fee)),
    }
    .ok_or(VaultError::Overflow)?;
    require!(vault.lamports() >= expected && vault.lamports() >= floor, VaultError::LamportsMoved);

    // What the coin cost is kept beside it, so that when it is sold the
    // profit or loss is known exactly. Fees on both sides count against it.
    let position = &mut ctx.accounts.position;
    position.agent = ctx.accounts.agent.id;
    position.mint = before.mint;
    position.bump = ctx.bumps.position;
    let realized = match side {
        Side::Buy => {
            position.cost = position.cost.checked_add(sol).and_then(|c| c.checked_add(fee)).ok_or(VaultError::Overflow)?;
            0
        }
        Side::Sell => {
            let released = portion(position.cost, tokens, before.amount);
            position.cost -= released;
            (sol - fee) as i64 - released as i64
        }
    };

    // Profit goes to the bank. When this sale takes the agent's realised
    // profit past its old high, what is new leaves the trading wallet: the
    // cut of gains to the pot that buys and burns $PIT, the rest to the
    // agent's bank. Below the old high nothing moves and nothing is cut, so a
    // loss is made back before anything is banked or charged.
    let total = ctx.accounts.agent.realized.checked_add(realized).ok_or(VaultError::Overflow)?;
    let fresh = total.saturating_sub(ctx.accounts.agent.banked_mark);
    let (mut banked, mut pit) = (0u64, 0u64);
    if side == Side::Sell && fresh > 0 {
        let bank = ctx.accounts.bank.to_account_info();
        let take = (fresh as u64).min(vault.lamports().saturating_sub(floor));
        let cut = half(share(take, config.terms.profit_fee_bps));
        // An empty bank has to be opened with at least its rent. A profit too
        // small for that waits in the wallet and goes over with the next one.
        if take > 0 && bank.lamports() + (take - cut) >= floor {
            if cut > 0 {
                system_program::transfer(CpiContext::new_with_signer(system.clone(), Transfer { from: vault.clone(), to: ctx.accounts.gains_pot.to_account_info() }, &[vault_seeds]), cut)?;
            }
            system_program::transfer(CpiContext::new_with_signer(system.clone(), Transfer { from: vault.clone(), to: bank }, &[vault_seeds]), take - cut)?;
            banked = take;
            pit = cut;
        }
    }

    let agent = &mut ctx.accounts.agent;
    agent.fills += 1;
    agent.volume = agent.volume.saturating_add(sol);
    agent.fees_paid = agent.fees_paid.saturating_add(fee).saturating_add(pit);
    agent.realized = total;
    agent.banked_mark += banked as i64;
    agent.unshared += banked;
    agent.banked += banked - pit;
    emit!(Filled { id: agent.id, buy: side == Side::Buy, mint: before.mint, sol, tokens, fee, realized, banked });

    // Sold out: the record has nothing left to say, and its rent goes back to
    // the trading key that paid for it.
    if side == Side::Sell && after.amount == 0 {
        ctx.accounts.position.close(ctx.accounts.executor.to_account_info())?;
    }
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, InitSpace)]
pub struct Terms {
    /// Signs spawns. It is the floor's record of which number is which agent.
    pub registrar: Pubkey,
    /// The trading key. It can ask for swaps and nothing else.
    pub executor: Pubkey,
    /// The one program a swap may go through.
    pub router: Pubkey,
    pub pit_mint: Pubkey,
    /// The least a spawn may burn, in the mint's smallest unit. The amount
    /// itself is set spawn by spawn by the registrar, to be worth a fixed sum
    /// in SOL. Keep this far under that, or a rising price shuts spawning
    /// until new terms apply. Zero keeps spawning shut.
    pub spawn_burn_min: u64,
    pub clearing_fee_bps: u16,
    /// The most of a vault's SOL one buy may spend.
    pub max_buy_bps: u16,
    pub treasury: Pubkey,
    pub burner: Pubkey,
    pub ecosystem: Pubkey,
    /// The ecosystem takes whatever these two leave.
    pub treasury_bps: u16,
    pub burn_bps: u16,
    /// The operator's share of an agent's newly banked profit, paid once a day.
    pub payout_bps: u16,
    /// The cut of gains above an agent's high water mark, taken as the gains
    /// are set aside in its bank. All of it goes to the burner, to buy $PIT
    /// and burn it.
    pub profit_fee_bps: u16,
    /// The least $PIT an operator must hold for their agent to pay half, in
    /// the mint's smallest unit. The amount itself is set by the registrar
    /// each time, to be worth a fixed sum in SOL. Keep this far under that.
    /// Zero keeps the holder class shut.
    pub holder_min: u64,
}

impl Terms {
    fn check(&self, admin: &Pubkey) -> Result<()> {
        // The cold key and the two hot keys must not be the same key, or the
        // separation they exist for is gone without anyone noticing.
        require!(self.executor != *admin && self.registrar != *admin, VaultError::RolesNotSeparate);
        require!(self.router != Pubkey::default() && self.router != crate::ID, VaultError::BadTerms);
        require!(self.clearing_fee_bps <= MAX_CLEARING_FEE_BPS, VaultError::FeeTooHigh);
        require!(self.payout_bps <= MAX_PAYOUT_BPS, VaultError::BadTerms);
        require!(self.profit_fee_bps <= MAX_PROFIT_FEE_BPS, VaultError::FeeTooHigh);
        require!(self.max_buy_bps > 0 && self.max_buy_bps <= MAX_BUY_BPS, VaultError::BadTerms);
        require!(self.treasury_bps as u32 + self.burn_bps as u32 <= BPS as u32, VaultError::BadTerms);
        Ok(())
    }
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
    pub terms: Terms,
    pub paused: bool,
    pub agents: u64,
    pub bump: u8,
    pub fees_bump: u8,
    pub gains_bump: u8,
    /// Terms that have been announced and are waiting out the delay, and when
    /// they were announced.
    pub has_pending: bool,
    pub pending_at: i64,
    pub pending: Terms,
}

#[account]
#[derive(InitSpace)]
pub struct Agent {
    pub id: u64,
    /// The wallet that burned $PIT for this agent. Withdrawals and the daily
    /// share of profit go here and nowhere else.
    pub operator: Pubkey,
    pub halted: bool,
    pub spawned_at: i64,
    pub fills: u64,
    pub volume: u64,
    pub fees_paid: u64,
    /// Everything closed trades have made or lost, fees included. Lamports.
    pub realized: i64,
    /// How much of that profit has already left the wallet for the bank. New
    /// profit is whatever is above this.
    pub banked_mark: i64,
    /// Banked profit the operator's share has not been paid on yet.
    pub unshared: u64,
    /// Everything that has ever reached the bank.
    pub banked: u64,
    pub paid_out: u64,
    pub last_payout: i64,
    pub bump: u8,
    pub vault_bump: u8,
    pub wsol_bump: u8,
    pub bank_bump: u8,
    /// Its fee class: the clearing fee on sells only. Set when the agent is
    /// made and not changeable by anyone.
    pub exit_fee_only: bool,
    /// Its operator holds $PIT, so it pays half the clearing fee and half the
    /// cut of gains. Granted and taken away by the registrar.
    pub holder: bool,
}

impl Agent {
    fn new(id: u64, operator: Pubkey, exit_fee_only: bool, bump: u8, vault_bump: u8, wsol_bump: u8, bank_bump: u8) -> Result<Self> {
        let now = Clock::get()?.unix_timestamp;
        Ok(Self { id, operator, halted: false, spawned_at: now, fills: 0, volume: 0, fees_paid: 0, realized: 0, banked_mark: 0, unshared: 0, banked: 0, paid_out: 0, last_payout: now, bump, vault_bump, wsol_bump, bank_bump, exit_fee_only, holder: false })
    }
}

/// What one coin in one agent's wallet cost, in lamports, fees included.
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub agent: u64,
    pub mint: Pubkey,
    pub cost: u64,
    pub bump: u8,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: where clearing fees collect. A program address with no data.
    #[account(mut, seeds = [b"fees"], bump)]
    pub fees: UncheckedAccount<'info>,
    /// CHECK: where the cut of gains collects, on its way to the burner.
    #[account(mut, seeds = [b"gains"], bump)]
    pub gains_pot: UncheckedAccount<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    /// The program's own data account, which names who may upgrade it.
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ VaultError::NotUpgradeAuthority
    )]
    pub program_data: Box<Account<'info, ProgramData>>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin @ VaultError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump, constraint = config.pending_admin == next.key() @ VaultError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    pub next: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct Spawn<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump, constraint = config.terms.registrar == registrar.key() @ VaultError::NotRegistrar)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = operator, space = 8 + Agent::INIT_SPACE, seeds = [b"agent", id.to_le_bytes().as_ref()], bump)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet. A program address with no data.
    #[account(seeds = [b"vault", id.to_le_bytes().as_ref()], bump)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: only derived here, so its bump is on record for every swap.
    #[account(seeds = [b"wsol", id.to_le_bytes().as_ref()], bump)]
    pub wsol: UncheckedAccount<'info>,
    /// CHECK: the agent's bank, where its profit is kept. Only derived here.
    #[account(seeds = [b"bank", id.to_le_bytes().as_ref()], bump)]
    pub bank: UncheckedAccount<'info>,
    #[account(mut)]
    pub operator: Signer<'info>,
    pub registrar: Signer<'info>,
    /// CHECK: must be the configured $PIT mint. Read by hand in the handler.
    #[account(mut, address = config.terms.pit_mint @ VaultError::WrongMint)]
    pub pit_mint: UncheckedAccount<'info>,
    /// CHECK: the operator's $PIT. The token program refuses the burn if it is not theirs.
    #[account(mut)]
    pub operator_pit: UncheckedAccount<'info>,
    /// CHECK: must be the token program that owns the mint. Checked in the handler.
    pub token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct Adopt<'info> {
    #[account(mut, seeds = [b"config"], bump = config.bump, has_one = admin @ VaultError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    #[account(init, payer = admin, space = 8 + Agent::INIT_SPACE, seeds = [b"agent", id.to_le_bytes().as_ref()], bump)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet. A program address with no data.
    #[account(seeds = [b"vault", id.to_le_bytes().as_ref()], bump)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: only derived here, so its bump is on record for every swap.
    #[account(seeds = [b"wsol", id.to_le_bytes().as_ref()], bump)]
    pub wsol: UncheckedAccount<'info>,
    /// CHECK: the agent's bank, where its profit is kept. Only derived here.
    #[account(seeds = [b"bank", id.to_le_bytes().as_ref()], bump)]
    pub bank: UncheckedAccount<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct GrantHolder<'info> {
    #[account(seeds = [b"config"], bump = config.bump, constraint = config.terms.registrar == registrar.key() @ VaultError::NotRegistrar)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump)]
    pub agent: Box<Account<'info, Agent>>,
    pub registrar: Signer<'info>,
    /// CHECK: the operator's $PIT. Read by hand in the handler, which checks
    /// the mint, the owner and the amount.
    pub operator_pit: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct RevokeHolder<'info> {
    #[account(seeds = [b"config"], bump = config.bump, constraint = config.terms.registrar == registrar.key() @ VaultError::NotRegistrar)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump)]
    pub agent: Box<Account<'info, Agent>>,
    pub registrar: Signer<'info>,
}

#[derive(Accounts)]
pub struct OperatorOnly<'info> {
    #[account(mut, seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump, has_one = operator @ VaultError::NotOperator)]
    pub agent: Box<Account<'info, Agent>>,
    pub operator: Signer<'info>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump, has_one = operator @ VaultError::NotOperator)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet, by its seeds.
    #[account(mut, seeds = [b"vault", agent.id.to_le_bytes().as_ref()], bump = agent.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    #[account(mut)]
    pub operator: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawToken<'info> {
    #[account(seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump, has_one = operator @ VaultError::NotOperator)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet, by its seeds.
    #[account(seeds = [b"vault", agent.id.to_le_bytes().as_ref()], bump = agent.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    pub operator: Signer<'info>,
    /// CHECK: a token account the vault owns. Read by hand in the handler.
    #[account(mut)]
    pub from: UncheckedAccount<'info>,
    /// CHECK: a token account the operator owns. Read by hand in the handler.
    #[account(mut)]
    pub to: UncheckedAccount<'info>,
    /// CHECK: the coin's mint. Both accounts are checked against it.
    pub mint: UncheckedAccount<'info>,
    /// The record of what this coin cost, if the agent bought it.
    #[account(mut, seeds = [b"position", agent.id.to_le_bytes().as_ref(), mint.key().as_ref()], bump = position.bump)]
    pub position: Option<Box<Account<'info, Position>>>,
    /// CHECK: must own the vault's token account. Checked in the handler.
    pub token_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct Payout<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump, has_one = operator @ VaultError::NotOperator)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's bank, by its seeds.
    #[account(mut, seeds = [b"bank", agent.id.to_le_bytes().as_ref()], bump = agent.bank_bump)]
    pub bank: UncheckedAccount<'info>,
    /// CHECK: the wallet that spawned the agent. It does not have to sign to be paid.
    #[account(mut)]
    pub operator: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct WithdrawBank<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump, has_one = operator @ VaultError::NotOperator)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's bank, by its seeds.
    #[account(mut, seeds = [b"bank", agent.id.to_le_bytes().as_ref()], bump = agent.bank_bump)]
    pub bank: UncheckedAccount<'info>,
    #[account(mut)]
    pub operator: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(seeds = [b"config"], bump = config.bump, constraint = config.terms.executor == executor.key() @ VaultError::NotExecutor)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet, by its seeds.
    #[account(mut, seeds = [b"vault", agent.id.to_le_bytes().as_ref()], bump = agent.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: wrapped SOL for the length of this instruction, by its seeds.
    #[account(mut, seeds = [b"wsol", agent.id.to_le_bytes().as_ref()], bump = agent.wsol_bump)]
    pub wsol: UncheckedAccount<'info>,
    /// CHECK: the wrapped SOL mint.
    #[account(address = NATIVE_MINT)]
    pub native_mint: UncheckedAccount<'info>,
    /// CHECK: the vault's token account for the coin. Read by hand in the handler.
    #[account(mut)]
    pub coin: UncheckedAccount<'info>,
    /// CHECK: the coin's mint. The handler holds the coin account to it.
    pub coin_mint: UncheckedAccount<'info>,
    /// What this coin has cost this agent so far.
    #[account(init_if_needed, payer = executor, space = 8 + Position::INIT_SPACE, seeds = [b"position", agent.id.to_le_bytes().as_ref(), coin_mint.key().as_ref()], bump)]
    pub position: Box<Account<'info, Position>>,
    /// CHECK: where clearing fees collect, by its seeds.
    #[account(mut, seeds = [b"fees"], bump = config.fees_bump)]
    pub fees: UncheckedAccount<'info>,
    /// CHECK: the agent's bank, by its seeds.
    #[account(mut, seeds = [b"bank", agent.id.to_le_bytes().as_ref()], bump = agent.bank_bump)]
    pub bank: UncheckedAccount<'info>,
    /// CHECK: where the cut of gains collects, by its seeds.
    #[account(mut, seeds = [b"gains"], bump = config.gains_bump)]
    pub gains_pot: UncheckedAccount<'info>,
    #[account(mut)]
    pub executor: Signer<'info>,
    /// CHECK: the one program a swap may go through.
    #[account(address = config.terms.router @ VaultError::WrongRouter)]
    pub router: UncheckedAccount<'info>,
    /// CHECK: wrapped SOL is always the original token program.
    #[account(address = TOKEN)]
    pub token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseToken<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [b"agent", agent.id.to_le_bytes().as_ref()], bump = agent.bump)]
    pub agent: Box<Account<'info, Agent>>,
    /// CHECK: the agent's wallet, by its seeds.
    #[account(seeds = [b"vault", agent.id.to_le_bytes().as_ref()], bump = agent.vault_bump)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: an empty token account the vault owns. Read by hand in the handler.
    #[account(mut)]
    pub account: UncheckedAccount<'info>,
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: must own the token account. Checked in the handler.
    pub token_program: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct CollectFees<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: where clearing fees collect, by its seeds.
    #[account(mut, seeds = [b"fees"], bump = config.fees_bump)]
    pub fees: UncheckedAccount<'info>,
    /// CHECK: where the cut of gains collects, by its seeds.
    #[account(mut, seeds = [b"gains"], bump = config.gains_bump)]
    pub gains_pot: UncheckedAccount<'info>,
    /// CHECK: fixed by the config.
    #[account(mut, address = config.terms.treasury @ VaultError::WrongDestination)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: fixed by the config.
    #[account(mut, address = config.terms.burner @ VaultError::WrongDestination)]
    pub burner: UncheckedAccount<'info>,
    /// CHECK: fixed by the config.
    #[account(mut, address = config.terms.ecosystem @ VaultError::WrongDestination)]
    pub ecosystem: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[event]
pub struct Spawned {
    pub id: u64,
    pub operator: Pubkey,
    pub vault: Pubkey,
    pub burned: u64,
}

#[event]
pub struct Filled {
    pub id: u64,
    pub buy: bool,
    pub mint: Pubkey,
    pub sol: u64,
    pub tokens: u64,
    pub fee: u64,
    /// What this fill realised. Zero on a buy.
    pub realized: i64,
    /// New profit this fill sent to the bank and the treasury together.
    pub banked: u64,
}

#[event]
pub struct Withdrawn {
    pub id: u64,
    pub lamports: u64,
}

#[event]
pub struct TermsProposed {
    pub at: i64,
    pub applies_at: i64,
}

#[event]
pub struct PaidOut {
    pub id: u64,
    pub operator: Pubkey,
    pub lamports: u64,
}

#[event]
pub struct HolderSet {
    pub id: u64,
    pub holder: bool,
}

#[event]
pub struct FeesCollected {
    pub treasury: u64,
    pub burn: u64,
    pub ecosystem: u64,
}

#[error_code]
pub enum VaultError {
    #[msg("Only the program's upgrade authority can set it up")]
    NotUpgradeAuthority,
    #[msg("Not the admin")]
    NotAdmin,
    #[msg("Not the registrar")]
    NotRegistrar,
    #[msg("Not the trading key")]
    NotExecutor,
    #[msg("Not this agent's operator")]
    NotOperator,
    #[msg("Not allowed")]
    NotAllowed,
    #[msg("The admin, the registrar and the trading key must be different keys")]
    RolesNotSeparate,
    #[msg("Those terms are out of bounds")]
    BadTerms,
    #[msg("That fee is above the ceiling")]
    FeeTooHigh,
    #[msg("The floor is paused")]
    Paused,
    #[msg("This agent is halted and can only sell")]
    Halted,
    #[msg("Spawning is not open yet")]
    SpawnClosed,
    #[msg("Wrong mint")]
    WrongMint,
    #[msg("Wrong token program")]
    WrongTokenProgram,
    #[msg("Wrong router")]
    WrongRouter,
    #[msg("Wrong destination")]
    WrongDestination,
    #[msg("Not a token account")]
    NotTokenAccount,
    #[msg("Not a mint")]
    NotMint,
    #[msg("That token account does not belong to this vault")]
    NotVaultAccount,
    #[msg("That token account does not belong to the operator")]
    NotOperatorAccount,
    #[msg("Wrapped SOL is not a coin to trade into")]
    NotACoin,
    #[msg("Nothing to move")]
    ZeroAmount,
    #[msg("Not enough in the vault")]
    InsufficientFunds,
    #[msg("A wallet is either empty or holds its rent")]
    BelowRent,
    #[msg("That buy is larger than one clip may be")]
    ClipTooLarge,
    #[msg("The wrapped SOL account is already in use")]
    WrappedBusy,
    #[msg("The router was shown a vault account it has no business with")]
    ForeignVaultAccount,
    #[msg("The vault itself was changed during the swap")]
    VaultTampered,
    #[msg("A vault token account was given a new owner or a delegate, or closed, during the swap")]
    AccountTampered,
    #[msg("The swap took more than it was allowed")]
    TookTooMuch,
    #[msg("The swap returned less than the minimum")]
    BelowMinimum,
    #[msg("Lamports left the vault that the swap does not account for")]
    LamportsMoved,
    #[msg("That token account is not empty")]
    NotEmpty,
    #[msg("Not enough fees to collect yet")]
    NothingToCollect,
    #[msg("Profit is shared once a day. It is not time yet")]
    TooSoon,
    #[msg("No new terms have been announced")]
    NoTermsPending,
    #[msg("New terms have to wait a day after they are announced")]
    TermsNotReady,
    #[msg("No new profit to share")]
    NothingToPay,
    #[msg("The bank holds back the share due at the next payout. The rest can come out")]
    BankHoldsNextPayout,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("That is under the least a spawn may burn")]
    BurnTooSmall,
    #[msg("The holder class is not open")]
    HolderClosed,
    #[msg("That is not enough $PIT to pay half")]
    HoldingTooSmall,
}
