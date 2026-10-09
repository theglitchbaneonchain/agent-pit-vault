//! The vault, driven through the compiled program.
//!
//! Everything worth knowing about this program is a statement about where
//! lamports and tokens are after an instruction, and who was able to make them
//! move. So every test here sends real transactions at the real binary and
//! then counts. The router is a stand in that can be told to behave or to try
//! each thing a hostile router could do with a vault's signature.

use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use mock_router::{DELEGATE_DESTINATION, DELEGATE_SOURCE, HONEST, PAY_ELSEWHERE, REOWN_DESTINATION, SEIZE_WALLET};
use pit_vault::{
    token::{initialize_account3, ACCOUNT_LEN, NATIVE_MINT, TOKEN, TOKEN_2022},
    Agent, Config, Position, Terms, VaultError, MIN_COLLECT,
};
use solana_sdk::{
    account::Account,
    bpf_loader_upgradeable,
    clock::Clock,
    instruction::{AccountMeta, Instruction},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};

const SOL: u64 = LAMPORTS_PER_SOL;
const SPAWN_BURN: u64 = 50_000; // what the registrar prices a spawn at in these tests, at six decimals
const SPAWN_BURN_MIN: u64 = 1_000; // the least the terms allow
const HOLDING: u64 = 400_000; // what the registrar says an operator must hold to pay half, in these tests
const HOLDER_MIN: u64 = 10_000; // the least the terms allow that to be
const FEE_BPS: u64 = 50;
const MAX_BUY_BPS: u64 = 2_500;
const MINT_LEN: usize = 82;
const DAY: i64 = 86_400;

struct Failure {
    err: String,
    logs: String,
}

/// The exact refusal, not just a refusal: "it failed" proves nothing about why.
fn refuses(result: Result<(), Failure>, why: VaultError, what: &str) {
    let code: u32 = why.into();
    match result {
        Ok(()) => panic!("{what}: went through, expected error {code}"),
        Err(f) => assert!(f.err.contains(&format!("Custom({code})")), "{what}: expected error {code}, got {}\n{}", f.err, f.logs),
    }
}

fn went(result: Result<(), Failure>, what: &str) {
    if let Err(f) = result {
        panic!("{what}: {}\n{}", f.err, f.logs)
    }
}

struct World {
    svm: LiteSVM,
    program: Pubkey,
    router: Pubkey,
    /// Pays every fee and every rent, so a balance under test moves only by
    /// what the instruction moved.
    payer: Keypair,
    admin: Keypair,
    registrar: Keypair,
    executor: Keypair,
    operator: Keypair,
    stranger: Keypair,
    treasury: Pubkey,
    burner: Pubkey,
    ecosystem: Pubkey,
    pit_mint: Pubkey,
    operator_pit: Pubkey,
    coin_program: Pubkey,
    coin_mint: Pubkey,
    pool: Pubkey,
    pool_wsol: Pubkey,
    pool_coin: Pubkey,
}

impl World {
    fn new() -> Self {
        Self::build(TOKEN, true)
    }

    fn build(coin_program: Pubkey, initialize: bool) -> Self {
        let mut svm = LiteSVM::new();
        let program = pit_vault::ID;
        svm.add_program(program, include_bytes!("../../../target/deploy/pit_vault.so"));
        let router = Pubkey::new_unique();
        svm.add_program(router, include_bytes!("../../../target/deploy/mock_router.so"));

        let keys: Vec<Keypair> = (0..6).map(|_| Keypair::new()).collect();
        for k in &keys {
            svm.airdrop(&k.pubkey(), 100_000 * SOL).unwrap();
        }
        let [payer, admin, registrar, executor, operator, stranger]: [Keypair; 6] = keys.try_into().ok().unwrap();

        // The loader's record of who may upgrade the program. The test SVM
        // loads programs without one, so it is written in by hand.
        let (program_data, _) = Pubkey::find_program_address(&[program.as_ref()], &bpf_loader_upgradeable::id());
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&0u64.to_le_bytes());
        data.push(1);
        data.extend_from_slice(admin.pubkey().as_ref());
        svm.set_account(program_data, Account { lamports: SOL, data, owner: bpf_loader_upgradeable::id(), executable: false, rent_epoch: 0 }).unwrap();

        if svm.get_account(&NATIVE_MINT).is_none() {
            let mut data = vec![0u8; MINT_LEN];
            data[44] = 9;
            data[45] = 1;
            svm.set_account(NATIVE_MINT, Account { lamports: SOL, data, owner: TOKEN, executable: false, rent_epoch: 0 }).unwrap();
        }

        let pool = Pubkey::find_program_address(&[b"pool"], &router).0;
        let mut w = World {
            svm,
            program,
            router,
            payer,
            admin,
            registrar,
            executor,
            operator,
            stranger,
            treasury: Pubkey::new_unique(),
            burner: Pubkey::new_unique(),
            ecosystem: Pubkey::new_unique(),
            pit_mint: Pubkey::default(),
            operator_pit: Pubkey::default(),
            coin_program,
            coin_mint: Pubkey::default(),
            pool,
            pool_wsol: Pubkey::default(),
            pool_coin: Pubkey::default(),
        };

        // $PIT is a Token 2022 mint, as a pump.fun launch would be.
        w.pit_mint = w.new_mint(TOKEN_2022, 6);
        w.operator_pit = w.new_token_account(TOKEN_2022, w.pit_mint, w.operator.pubkey(), 0);
        w.mint_to(TOKEN_2022, w.pit_mint, w.operator_pit, 1_000_000);

        w.coin_mint = w.new_mint(coin_program, 6);
        w.pool_coin = w.new_token_account(coin_program, w.coin_mint, pool, 0);
        w.mint_to(coin_program, w.coin_mint, w.pool_coin, 1_000_000_000_000);
        w.pool_wsol = w.new_token_account(TOKEN, NATIVE_MINT, pool, 5_000 * SOL);

        if initialize {
            let admin = w.admin.insecure_clone();
            went(w.initialize(&admin), "initialize");
        }
        w
    }

    fn pda(&self, seeds: &[&[u8]]) -> Pubkey {
        Pubkey::find_program_address(seeds, &self.program).0
    }
    fn config(&self) -> Pubkey {
        self.pda(&[b"config"])
    }
    fn fees(&self) -> Pubkey {
        self.pda(&[b"fees"])
    }
    fn agent(&self, id: u64) -> Pubkey {
        self.pda(&[b"agent", &id.to_le_bytes()])
    }
    fn vault(&self, id: u64) -> Pubkey {
        self.pda(&[b"vault", &id.to_le_bytes()])
    }
    fn wsol(&self, id: u64) -> Pubkey {
        self.pda(&[b"wsol", &id.to_le_bytes()])
    }
    fn bank(&self, id: u64) -> Pubkey {
        self.pda(&[b"bank", &id.to_le_bytes()])
    }
    /// Where the cut of gains waits on its way to the burner.
    fn pot(&self) -> Pubkey {
        self.pda(&[b"gains"])
    }
    fn position(&self, id: u64) -> Pubkey {
        self.pda(&[b"position", &id.to_le_bytes(), self.coin_mint.as_ref()])
    }
    fn position_cost(&self, id: u64) -> Option<u64> {
        let a = self.svm.get_account(&self.position(id)).filter(|a| a.lamports > 0)?;
        Some(Position::try_deserialize(&mut a.data.as_slice()).unwrap().cost)
    }
    /// A day cannot be waited out in a test, so the clock is moved instead.
    fn warp(&mut self, seconds: i64) {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.unix_timestamp += seconds;
        self.svm.set_sysvar::<Clock>(&clock);
    }
    fn lamports(&self, key: &Pubkey) -> u64 {
        self.svm.get_balance(key).unwrap_or(0)
    }
    fn tokens(&self, key: &Pubkey) -> u64 {
        let a = self.svm.get_account(key).expect("token account");
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    }
    fn rent(&self, len: usize) -> u64 {
        self.svm.minimum_balance_for_rent_exemption(len)
    }
    fn agent_state(&self, id: u64) -> Agent {
        Agent::try_deserialize(&mut self.svm.get_account(&self.agent(id)).expect("agent").data.as_slice()).unwrap()
    }
    fn config_state(&self) -> Config {
        Config::try_deserialize(&mut self.svm.get_account(&self.config()).expect("config").data.as_slice()).unwrap()
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), Failure> {
        let payer = self.payer.insecure_clone();
        // A fresh blockhash each time, so the same call can be sent twice.
        self.svm.expire_blockhash();
        let mut all: Vec<&Keypair> = vec![&payer];
        for s in signers {
            if !all.iter().any(|k| k.pubkey() == s.pubkey()) {
                all.push(s);
            }
        }
        let tx = Transaction::new_signed_with_payer(ixs, Some(&payer.pubkey()), &all, self.svm.latest_blockhash());
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| Failure { err: format!("{:?}", e.err), logs: e.meta.logs.join("\n") })
    }

    fn new_mint(&mut self, program: Pubkey, decimals: u8) -> Pubkey {
        let mint = Keypair::new();
        let payer = self.payer.insecure_clone();
        let mut data = vec![20, decimals];
        data.extend_from_slice(payer.pubkey().as_ref());
        data.push(0);
        let ixs = [
            system_instruction::create_account(&payer.pubkey(), &mint.pubkey(), self.rent(MINT_LEN), MINT_LEN as u64, &program),
            Instruction { program_id: program, accounts: vec![AccountMeta::new(mint.pubkey(), false)], data },
        ];
        went(self.send(&ixs, &[&mint]), "new mint");
        mint.pubkey()
    }

    /// A token account for `owner`. For wrapped SOL, `wrapped` is its balance.
    fn new_token_account(&mut self, program: Pubkey, mint: Pubkey, owner: Pubkey, wrapped: u64) -> Pubkey {
        let account = Keypair::new();
        let payer = self.payer.insecure_clone();
        let ixs = [
            system_instruction::create_account(&payer.pubkey(), &account.pubkey(), self.rent(ACCOUNT_LEN) + wrapped, ACCOUNT_LEN as u64, &program),
            initialize_account3(&program, &account.pubkey(), &mint, &owner),
        ];
        went(self.send(&ixs, &[&account]), "new token account");
        account.pubkey()
    }

    fn mint_to(&mut self, program: Pubkey, mint: Pubkey, to: Pubkey, amount: u64) {
        let payer = self.payer.insecure_clone();
        let mut data = vec![7];
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction { program_id: program, accounts: vec![AccountMeta::new(mint, false), AccountMeta::new(to, false), AccountMeta::new_readonly(payer.pubkey(), true)], data };
        went(self.send(&[ix], &[]), "mint to");
    }

    fn terms(&self) -> Terms {
        Terms {
            registrar: self.registrar.pubkey(),
            executor: self.executor.pubkey(),
            router: self.router,
            pit_mint: self.pit_mint,
            spawn_burn_min: SPAWN_BURN_MIN,
            clearing_fee_bps: FEE_BPS as u16,
            max_buy_bps: MAX_BUY_BPS as u16,
            treasury: self.treasury,
            burner: self.burner,
            ecosystem: self.ecosystem,
            treasury_bps: 6_000,
            burn_bps: 2_500,
            payout_bps: 2_000,
            profit_fee_bps: 100,
            holder_min: HOLDER_MIN,
        }
    }

    fn initialize(&mut self, by: &Keypair) -> Result<(), Failure> {
        let program_data = Pubkey::find_program_address(&[self.program.as_ref()], &bpf_loader_upgradeable::id()).0;
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::Initialize { config: self.config(), fees: self.fees(), gains_pot: self.pot(), admin: by.pubkey(), program_data, system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::Initialize { terms: self.terms() }.data(),
        };
        self.send(&[ix], &[by])
    }

    fn admin_ix(&mut self, by: &Keypair, data: Vec<u8>) -> Result<(), Failure> {
        let ix = Instruction { program_id: self.program, accounts: pit_vault::accounts::AdminOnly { config: self.config(), admin: by.pubkey() }.to_account_metas(None), data };
        self.send(&[ix], &[by])
    }
    fn propose(&mut self, by: &Keypair, terms: Terms) -> Result<(), Failure> {
        self.admin_ix(by, pit_vault::instruction::ProposeTerms { terms }.data())
    }
    fn apply(&mut self, by: &Keypair) -> Result<(), Failure> {
        self.admin_ix(by, pit_vault::instruction::ApplyTerms {}.data())
    }
    fn cancel(&mut self, by: &Keypair) -> Result<(), Failure> {
        self.admin_ix(by, pit_vault::instruction::CancelTerms {}.data())
    }

    fn set_paused(&mut self, paused: bool) {
        let admin = self.admin.insecure_clone();
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::AdminOnly { config: self.config(), admin: admin.pubkey() }.to_account_metas(None),
            data: pit_vault::instruction::SetPaused { paused }.data(),
        };
        went(self.send(&[ix], &[&admin]), "set paused");
    }

    fn spawn_as(&mut self, id: u64, operator: &Keypair, operator_pit: Pubkey, registrar: &Keypair) -> Result<(), Failure> {
        self.spawn_burning(id, SPAWN_BURN, operator, operator_pit, registrar)
    }

    fn spawn_burning(&mut self, id: u64, burn: u64, operator: &Keypair, operator_pit: Pubkey, registrar: &Keypair) -> Result<(), Failure> {
        self.spawn_class(id, burn, false, operator, operator_pit, registrar)
    }

    fn spawn_class(&mut self, id: u64, burn: u64, exit_fee_only: bool, operator: &Keypair, operator_pit: Pubkey, registrar: &Keypair) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::Spawn {
                config: self.config(),
                agent: self.agent(id),
                vault: self.vault(id),
                wsol: self.wsol(id),
                bank: self.bank(id),
                operator: operator.pubkey(),
                registrar: registrar.pubkey(),
                pit_mint: self.pit_mint,
                operator_pit,
                token_program: TOKEN_2022,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: pit_vault::instruction::Spawn { id, burn, exit_fee_only }.data(),
        };
        self.send(&[ix], &[operator, registrar])
    }

    /// The registrar puts an agent in the holder class, against a $PIT account.
    fn grant_holder(&mut self, id: u64, min: u64, operator_pit: Pubkey, registrar: &Keypair) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::GrantHolder { config: self.config(), agent: self.agent(id), registrar: registrar.pubkey(), operator_pit }.to_account_metas(None),
            data: pit_vault::instruction::GrantHolder { min }.data(),
        };
        self.send(&[ix], &[registrar])
    }

    fn revoke_holder(&mut self, id: u64, registrar: &Keypair) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::RevokeHolder { config: self.config(), agent: self.agent(id), registrar: registrar.pubkey() }.to_account_metas(None),
            data: pit_vault::instruction::RevokeHolder {}.data(),
        };
        self.send(&[ix], &[registrar])
    }

    fn spawn(&mut self, id: u64) -> Result<(), Failure> {
        let (operator, registrar) = (self.operator.insecure_clone(), self.registrar.insecure_clone());
        self.spawn_as(id, &operator, self.operator_pit, &registrar)
    }

    /// An agent on the floor with SOL in its wallet and an empty account for the coin.
    fn funded(&mut self, id: u64, sol: u64) -> Pubkey {
        went(self.spawn(id), "spawn");
        let payer = self.payer.insecure_clone();
        went(self.send(&[system_instruction::transfer(&payer.pubkey(), &self.vault(id), sol)], &[]), "fund");
        self.new_token_account(self.coin_program, self.coin_mint, self.vault(id), 0)
    }

    fn withdraw(&mut self, id: u64, by: &Keypair, lamports: u64) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::Withdraw { agent: self.agent(id), vault: self.vault(id), operator: by.pubkey(), system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::Withdraw { lamports }.data(),
        };
        self.send(&[ix], &[by])
    }

    /// Anyone can turn the payout crank. Nobody signs but the fee payer.
    fn payout(&mut self, id: u64, to: Pubkey) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::Payout { config: self.config(), agent: self.agent(id), bank: self.bank(id), operator: to, system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::Payout {}.data(),
        };
        self.send(&[ix], &[])
    }

    fn withdraw_bank(&mut self, id: u64, by: &Keypair, lamports: u64) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::WithdrawBank { config: self.config(), agent: self.agent(id), bank: self.bank(id), operator: by.pubkey(), system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::WithdrawBank { lamports }.data(),
        };
        self.send(&[ix], &[by])
    }

    fn set_halted(&mut self, id: u64, by: &Keypair, halted: bool) -> Result<(), Failure> {
        let ix = Instruction {
            program_id: self.program,
            accounts: pit_vault::accounts::OperatorOnly { agent: self.agent(id), operator: by.pubkey() }.to_account_metas(None),
            data: pit_vault::instruction::SetHalted { halted }.data(),
        };
        self.send(&[ix], &[by])
    }

    /// A swap as the trading key would send it. `take` and `give` are what the
    /// stand in router will pull from the source and pay to the destination.
    #[allow(clippy::too_many_arguments)]
    fn swap_with(&mut self, id: u64, buy: bool, coin: Pubkey, amount_in: u64, min_out: u64, mode: u8, take: u64, give: u64, spare: Option<AccountMeta>, by: &Keypair, router: Pubkey) -> Result<(), Failure> {
        let (vault, wsol) = (self.vault(id), self.wsol(id));
        let (source, destination, pool_in, pool_out, token_in, token_out) =
            if buy { (wsol, coin, self.pool_wsol, self.pool_coin, TOKEN, self.coin_program) } else { (coin, wsol, self.pool_coin, self.pool_wsol, self.coin_program, TOKEN) };
        let mut accounts = pit_vault::accounts::Swap {
            config: self.config(),
            agent: self.agent(id),
            vault,
            wsol,
            native_mint: NATIVE_MINT,
            coin,
            coin_mint: self.coin_mint,
            position: self.position(id),
            fees: self.fees(),
            bank: self.bank(id),
            gains_pot: self.pot(),
            executor: by.pubkey(),
            router,
            token_program: TOKEN,
            system_program: system_program::id(),
        }
        .to_account_metas(None);
        accounts.extend([
            AccountMeta::new(source, false),
            AccountMeta::new(destination, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(pool_in, false),
            AccountMeta::new(pool_out, false),
            AccountMeta::new_readonly(self.pool, false),
            AccountMeta::new_readonly(token_in, false),
            AccountMeta::new_readonly(token_out, false),
        ]);
        accounts.extend(spare);
        let mut route = vec![mode];
        route.extend_from_slice(&take.to_le_bytes());
        route.extend_from_slice(&give.to_le_bytes());
        let data = if buy { pit_vault::instruction::Buy { amount_in, min_out, data: route }.data() } else { pit_vault::instruction::Sell { amount_in, min_out, data: route }.data() };
        self.send(&[Instruction { program_id: self.program, accounts, data }], &[by])
    }

    fn buy(&mut self, id: u64, coin: Pubkey, sol: u64, tokens: u64) -> Result<(), Failure> {
        let (executor, router) = (self.executor.insecure_clone(), self.router);
        self.swap_with(id, true, coin, sol, tokens, HONEST, sol, tokens, None, &executor, router)
    }

    fn sell(&mut self, id: u64, coin: Pubkey, tokens: u64, sol: u64) -> Result<(), Failure> {
        let (executor, router) = (self.executor.insecure_clone(), self.router);
        self.swap_with(id, false, coin, tokens, sol, HONEST, tokens, sol, None, &executor, router)
    }

    /// A buy routed through a router that also tries something.
    fn hostile_buy(&mut self, id: u64, coin: Pubkey, mode: u8, spare: Option<AccountMeta>) -> Result<(), Failure> {
        let (executor, router) = (self.executor.insecure_clone(), self.router);
        self.swap_with(id, true, coin, SOL, 1, mode, SOL, 1_000_000, spare, &executor, router)
    }
}

const FEE_ON_ONE_SOL: u64 = SOL * FEE_BPS / 10_000;

#[test]
fn only_the_upgrade_authority_can_set_the_program_up() {
    let mut w = World::build(TOKEN, false);
    let stranger = w.stranger.insecure_clone();
    refuses(w.initialize(&stranger), VaultError::NotUpgradeAuthority, "a stranger initialising");
    let admin = w.admin.insecure_clone();
    went(w.initialize(&admin), "the upgrade authority initialising");
    assert!(w.initialize(&admin).is_err(), "initialising twice");
    assert_eq!(w.config_state().admin, admin.pubkey());
    assert_eq!(w.lamports(&w.fees()), w.rent(0), "the fee account starts holding exactly its rent");
    assert_eq!(w.lamports(&w.pot()), w.rent(0), "and so does the treasury's");
}

#[test]
fn wallet_addresses_are_the_ones_the_floor_already_shows() {
    // The floor server derives these without this program. If the two ever
    // disagree, every address on the site is wrong.
    let w = World::new();
    assert_eq!(w.vault(1).to_string(), "4tXAVd4QpuVMJ7keHKheX4hxkJqePjzRqRaanQL3eAaR");
    assert_eq!(w.vault(7).to_string(), "BT2cQkdG1EUU6xfNnivfzNU3D4YLmMP926ZW38as2DQQ");
    assert_eq!(w.vault(300).to_string(), "HdLbzaRS82aNr6mue5ehio6hsyML2pxtAR5Cu32J4tP1");
}

#[test]
fn spawning_burns_pit_and_registers_the_agent() {
    let mut w = World::new();
    let supply = |w: &World| u64::from_le_bytes(w.svm.get_account(&w.pit_mint).unwrap().data[36..44].try_into().unwrap());
    let before = supply(&w);

    went(w.spawn(6), "spawn");
    assert_eq!(w.tokens(&w.operator_pit), 1_000_000 - SPAWN_BURN, "the operator's $PIT");
    assert_eq!(supply(&w), before - SPAWN_BURN, "burned, not moved");
    let a = w.agent_state(6);
    assert_eq!((a.id, a.operator, a.halted, a.fills, a.realized, a.paid_out), (6, w.operator.pubkey(), false, 0, 0, 0));
    assert_eq!(w.config_state().agents, 1);

    assert!(w.spawn(6).is_err(), "the same number twice");

    let (operator, registrar, stranger) = (w.operator.insecure_clone(), w.registrar.insecure_clone(), w.stranger.insecure_clone());
    refuses(w.spawn_as(7, &operator, w.operator_pit, &stranger), VaultError::NotRegistrar, "a spawn the registrar did not sign");

    // Someone with no $PIT cannot spawn, and cannot burn someone else's.
    let empty = w.new_token_account(TOKEN_2022, w.pit_mint, stranger.pubkey(), 0);
    assert!(w.spawn_as(7, &stranger, empty, &registrar).is_err(), "spawning with no $PIT");
    assert!(w.spawn_as(7, &stranger, w.operator_pit, &registrar).is_err(), "burning the operator's $PIT");
    assert_eq!(w.tokens(&w.operator_pit), 1_000_000 - SPAWN_BURN);
}

#[test]
fn a_spawn_burns_what_the_registrar_priced_it_at() {
    let mut w = World::new();
    let (operator, registrar) = (w.operator.insecure_clone(), w.registrar.insecure_clone());
    let supply = |w: &World| u64::from_le_bytes(w.svm.get_account(&w.pit_mint).unwrap().data[36..44].try_into().unwrap());
    let before = supply(&w);

    // The price fell, so the same sum in SOL is more $PIT.
    went(w.spawn_burning(6, 180_000, &operator, w.operator_pit, &registrar), "a spawn at a low price");
    assert_eq!(w.tokens(&w.operator_pit), 1_000_000 - 180_000);
    // The price rose, so it is fewer.
    went(w.spawn_burning(7, 4_000, &operator, w.operator_pit, &registrar), "a spawn at a high price");
    assert_eq!(w.tokens(&w.operator_pit), 1_000_000 - 180_000 - 4_000);
    assert_eq!(supply(&w), before - 184_000, "burned, not moved");

    // Never under the least the terms allow, and never nothing.
    went(w.spawn_burning(8, SPAWN_BURN_MIN, &operator, w.operator_pit, &registrar), "a spawn at exactly the least");
    refuses(w.spawn_burning(9, SPAWN_BURN_MIN - 1, &operator, w.operator_pit, &registrar), VaultError::BurnTooSmall, "a burn under the least");
    refuses(w.spawn_burning(9, 0, &operator, w.operator_pit, &registrar), VaultError::BurnTooSmall, "a spawn that burns nothing");

    // The amount is the registrar's to set. The operator cannot name their own.
    let stranger = w.stranger.insecure_clone();
    refuses(w.spawn_burning(9, SPAWN_BURN_MIN, &operator, w.operator_pit, &stranger), VaultError::NotRegistrar, "a cheap spawn the registrar did not sign");
    // More than the operator holds does not go through.
    assert!(w.spawn_burning(9, 10_000_000, &operator, w.operator_pit, &registrar).is_err(), "burning more than is held");
    assert_eq!(w.config_state().agents, 3);
}

#[test]
fn the_admin_can_register_an_agent_without_a_burn() {
    let mut w = World::new();
    let adopt = |w: &mut World, by: &Keypair, id: u64| {
        let ix = Instruction {
            program_id: w.program,
            accounts: pit_vault::accounts::Adopt { config: w.config(), agent: w.agent(id), vault: w.vault(id), wsol: w.wsol(id), bank: w.bank(id), admin: by.pubkey(), system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::Adopt { id, operator: w.operator.pubkey(), exit_fee_only: false }.data(),
        };
        w.send(&[ix], &[by])
    };
    let (admin, stranger) = (w.admin.insecure_clone(), w.stranger.insecure_clone());
    refuses(adopt(&mut w, &stranger, 1), VaultError::NotAdmin, "a stranger adopting");
    went(adopt(&mut w, &admin, 1), "the admin adopting");
    assert_eq!(w.agent_state(1).operator, w.operator.pubkey());
    assert_eq!(w.tokens(&w.operator_pit), 1_000_000, "nothing burned");
}

#[test]
fn only_the_operator_can_take_sol_out() {
    let mut w = World::new();
    w.funded(6, 10 * SOL);
    let vault = w.vault(6);
    let (operator, stranger, executor, admin, registrar) = (w.operator.insecure_clone(), w.stranger.insecure_clone(), w.executor.insecure_clone(), w.admin.insecure_clone(), w.registrar.insecure_clone());

    for (who, name) in [(&stranger, "a stranger"), (&executor, "the trading key"), (&admin, "the admin"), (&registrar, "the registrar")] {
        refuses(w.withdraw(6, who, SOL), VaultError::NotOperator, &format!("{name} withdrawing"));
    }
    assert_eq!(w.lamports(&vault), 10 * SOL);

    let before = w.lamports(&operator.pubkey());
    went(w.withdraw(6, &operator, 4 * SOL), "the operator withdrawing");
    assert_eq!(w.lamports(&operator.pubkey()), before + 4 * SOL, "all of it reached the operator");
    assert_eq!(w.lamports(&vault), 6 * SOL);

    refuses(w.withdraw(6, &operator, 7 * SOL), VaultError::InsufficientFunds, "more than is there");
    refuses(w.withdraw(6, &operator, 6 * SOL - 1), VaultError::BelowRent, "leaving dust below rent");

    // A paused floor does not hold anyone's money.
    w.set_paused(true);
    went(w.withdraw(6, &operator, u64::MAX), "taking everything while paused");
    assert_eq!(w.lamports(&vault), 0);
    assert_eq!(w.lamports(&operator.pubkey()), before + 10 * SOL);
}

#[test]
fn a_buy_and_a_sell_close_to_the_lamport() {
    for coin_program in [TOKEN, TOKEN_2022] {
        let mut w = World::build(coin_program, true);
        let coin = w.funded(6, 10 * SOL);
        let (vault, fees) = (w.vault(6), w.fees());
        let fees_before = w.lamports(&fees);

        went(w.buy(6, coin, SOL, 1_000_000), "buy");
        assert_eq!(w.lamports(&vault), 10 * SOL - SOL - FEE_ON_ONE_SOL, "down by the clip and the fee, nothing else");
        assert_eq!(w.tokens(&coin), 1_000_000);
        assert_eq!(w.lamports(&fees), fees_before + FEE_ON_ONE_SOL);
        assert_eq!(w.lamports(&w.wsol(6)), 0, "wrapped SOL does not outlive the instruction");

        // The router fills less than it was offered: only what it took is spent.
        let (executor, router) = (w.executor.insecure_clone(), w.router);
        let had = w.lamports(&vault);
        went(w.swap_with(6, true, coin, SOL, 1, HONEST, SOL * 7 / 10, 500_000, None, &executor, router), "partial buy");
        let spent = SOL * 7 / 10;
        assert_eq!(w.lamports(&vault), had - spent - spent * FEE_BPS / 10_000);
        assert_eq!(w.tokens(&coin), 1_500_000);

        let had = w.lamports(&vault);
        let proceeds = SOL * 6 / 10;
        went(w.sell(6, coin, 400_000, proceeds), "sell");
        // 1.5M coins cost 1.7085 SOL with fees. 400K of them went for 0.6.
        let profit = proceeds - proceeds * FEE_BPS / 10_000 - (SOL + FEE_ON_ONE_SOL + spent + spent * FEE_BPS / 10_000) * 4 / 15;
        let (bank, pot) = (w.lamports(&w.bank(6)), w.lamports(&w.pot()) - w.rent(0));
        assert_eq!(w.agent_state(6).realized, profit as i64);
        assert_eq!((bank, pot), (profit - profit / 100, profit / 100), "the profit went to the bank, less the pit's hundredth");
        assert_eq!(w.lamports(&vault) + bank + pot, had + proceeds - proceeds * FEE_BPS / 10_000, "and every lamport of the sale is somewhere");
        assert_eq!(w.tokens(&coin), 1_100_000);
        assert_eq!(w.lamports(&w.wsol(6)), 0);

        let a = w.agent_state(6);
        assert_eq!(a.fills, 3);
        assert_eq!(a.volume, SOL + spent + proceeds);
        assert_eq!(a.fees_paid, w.lamports(&fees) - fees_before + pot);
    }
}

#[test]
fn an_exit_fee_agent_pays_the_clearing_fee_on_the_way_out_only() {
    let mut w = World::new();
    let (operator, registrar) = (w.operator.insecure_clone(), w.registrar.insecure_clone());
    went(w.spawn_class(6, SPAWN_BURN, true, &operator, w.operator_pit, &registrar), "spawn in the exit fee class");
    assert!(w.agent_state(6).exit_fee_only);
    let payer = w.payer.insecure_clone();
    went(w.send(&[system_instruction::transfer(&payer.pubkey(), &w.vault(6), 10 * SOL)], &[]), "fund");
    let coin = w.new_token_account(w.coin_program, w.coin_mint, w.vault(6), 0);
    let (vault, fees) = (w.vault(6), w.fees());
    let fees_before = w.lamports(&fees);

    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    assert_eq!(w.lamports(&vault), 10 * SOL - SOL, "down by the clip and not a lamport more");
    assert_eq!(w.lamports(&fees), fees_before, "no fee on the way in");

    let proceeds = SOL * 12 / 10;
    went(w.sell(6, coin, 1_000_000, proceeds), "sell");
    let fee = proceeds * FEE_BPS / 10_000;
    assert_eq!(w.lamports(&fees), fees_before + fee, "the fee is taken on the way out");
    // What it made is the proceeds less the one fee, over what it paid.
    assert_eq!(w.agent_state(6).realized, (proceeds - fee - SOL) as i64);

    // An ordinary agent beside it pays both ways, as before.
    went(w.spawn(7), "an ordinary spawn");
    assert!(!w.agent_state(7).exit_fee_only);
}

#[test]
fn nobody_can_point_an_agents_money_anywhere_else() {
    let mut w = World::new();
    w.funded(6, 10 * SOL);
    let (admin, stranger) = (w.admin.insecure_clone(), w.stranger.insecure_clone());
    let first = w.agent_state(6).operator;

    // There is no instruction that changes who an agent belongs to. The only
    // one that names an operator makes a new agent, and it will not write over
    // one that exists, whoever asks.
    let readopt = |w: &mut World, by: &Keypair| {
        let ix = Instruction {
            program_id: w.program,
            accounts: pit_vault::accounts::Adopt { config: w.config(), agent: w.agent(6), vault: w.vault(6), wsol: w.wsol(6), bank: w.bank(6), admin: by.pubkey(), system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::Adopt { id: 6, operator: by.pubkey(), exit_fee_only: false }.data(),
        };
        w.send(&[ix], &[by])
    };
    assert!(readopt(&mut w, &admin).is_err(), "the admin taking an agent over");
    assert!(readopt(&mut w, &stranger).is_err(), "a stranger taking an agent over");
    assert_eq!(w.agent_state(6).operator, first, "it still belongs to the wallet that spawned it");

    // And with the agent still theirs, nobody else can take from it.
    refuses(w.withdraw(6, &admin, SOL), VaultError::NotOperator, "the admin withdrawing");
    assert_eq!(w.lamports(&w.vault(6)), 10 * SOL);
}

#[test]
fn only_the_trading_key_trades_and_only_through_the_router() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (operator, stranger, admin, executor, router) = (w.operator.insecure_clone(), w.stranger.insecure_clone(), w.admin.insecure_clone(), w.executor.insecure_clone(), w.router);
    for (who, name) in [(&stranger, "a stranger"), (&operator, "the operator"), (&admin, "the admin")] {
        refuses(w.swap_with(6, true, coin, SOL, 1, HONEST, SOL, 1_000_000, None, who, router), VaultError::NotExecutor, &format!("{name} trading"));
    }
    refuses(w.swap_with(6, true, coin, SOL, 1, HONEST, SOL, 1_000_000, None, &executor, TOKEN), VaultError::WrongRouter, "a swap through some other program");
    assert_eq!(w.lamports(&w.vault(6)), 10 * SOL);
}

#[test]
fn a_hostile_router_gets_nothing() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let vault = w.vault(6);
    let thief = w.new_token_account(TOKEN, w.coin_mint, w.stranger.pubkey(), 0);

    // It takes the SOL and pays the coins to someone else.
    refuses(w.hostile_buy(6, coin, PAY_ELSEWHERE, Some(AccountMeta::new(thief, false))), VaultError::BelowMinimum, "output paid elsewhere");
    // It swaps honestly, then leaves itself a way back in.
    refuses(w.hostile_buy(6, coin, DELEGATE_DESTINATION, None), VaultError::AccountTampered, "a delegate left on the coin account");
    refuses(w.hostile_buy(6, coin, REOWN_DESTINATION, None), VaultError::AccountTampered, "the coin account handed to a new owner");
    // It uses the wallet's signature to make itself the wallet's owner.
    refuses(w.hostile_buy(6, coin, SEIZE_WALLET, Some(AccountMeta::new_readonly(system_program::id(), false))), VaultError::VaultTampered, "the wallet assigned to the router");
    // It is shown another of the vault's holdings.
    let other_mint = w.new_mint(TOKEN, 6);
    let other = w.new_token_account(TOKEN, other_mint, vault, 0);
    refuses(w.hostile_buy(6, coin, HONEST, Some(AccountMeta::new(other, false))), VaultError::ForeignVaultAccount, "a second vault holding in the router's reach");

    assert_eq!(w.lamports(&vault), 10 * SOL, "after all of that, not a lamport has moved");
    assert_eq!(w.tokens(&coin), 0);
    assert_eq!(w.tokens(&thief), 0);

    // The same on the way out.
    went(w.buy(6, coin, SOL, 1_000_000), "an honest buy");
    let (executor, router) = (w.executor.insecure_clone(), w.router);
    let had = w.lamports(&vault);
    refuses(w.swap_with(6, false, coin, 100_000, 1, HONEST, 250_000, SOL / 10, None, &executor, router), VaultError::TookTooMuch, "a sell that took more coins than it was given");
    refuses(w.swap_with(6, false, coin, 100_000, 1, DELEGATE_SOURCE, 100_000, SOL / 10, None, &executor, router), VaultError::AccountTampered, "a delegate left on the way out");
    refuses(w.swap_with(6, false, coin, 100_000, SOL / 10 + 1, HONEST, 100_000, SOL / 10, None, &executor, router), VaultError::BelowMinimum, "a sell under its minimum");
    refuses(w.swap_with(6, false, coin, 100_000, 0, HONEST, 100_000, 0, None, &executor, router), VaultError::BelowMinimum, "a sell for nothing");
    assert_eq!(w.lamports(&vault), had);
    assert_eq!(w.tokens(&coin), 1_000_000);
}

#[test]
fn a_swap_only_touches_the_vaults_own_coin_account() {
    let mut w = World::new();
    w.funded(6, 10 * SOL);
    let vault = w.vault(6);
    let (executor, router) = (w.executor.insecure_clone(), w.router);

    let not_ours = w.new_token_account(TOKEN, w.coin_mint, w.stranger.pubkey(), 0);
    refuses(w.swap_with(6, true, not_ours, SOL, 1, HONEST, SOL, 1_000_000, None, &executor, router), VaultError::NotVaultAccount, "buying into someone else's account");
    let wrapped = w.new_token_account(TOKEN, NATIVE_MINT, vault, 0);
    refuses(w.swap_with(6, true, wrapped, SOL, 1, HONEST, SOL, 1_000_000, None, &executor, router), VaultError::NotACoin, "wrapped SOL passed off as the coin");
    refuses(w.swap_with(6, true, w.pit_mint, SOL, 1, HONEST, SOL, 1_000_000, None, &executor, router), VaultError::NotTokenAccount, "a mint passed off as the coin account");
    // Each coin's cost is kept under its own mint. A fill booked under another
    // coin's record would make the profit figure a lie.
    let other_mint = w.new_mint(TOKEN, 6);
    let other = w.new_token_account(TOKEN, other_mint, vault, 0);
    refuses(w.swap_with(6, true, other, SOL, 1, HONEST, SOL, 1_000_000, None, &executor, router), VaultError::WrongMint, "a coin booked under another coin's record");
}

#[test]
fn one_buy_cannot_spend_more_than_a_clip() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let cap = (10 * SOL - w.rent(0)) * MAX_BUY_BPS / 10_000;
    refuses(w.buy(6, coin, cap + 1, 1_000_000), VaultError::ClipTooLarge, "a buy one lamport over the cap");
    went(w.buy(6, coin, cap, 1_000_000), "a buy at the cap");
    refuses(w.buy(6, coin, 0, 1_000_000), VaultError::ZeroAmount, "a buy of nothing");
}

#[test]
fn the_brakes_work() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    let (operator, stranger) = (w.operator.insecure_clone(), w.stranger.insecure_clone());

    // The operator's halt: no new buys, selling still allowed.
    refuses(w.set_halted(6, &stranger, true), VaultError::NotOperator, "a stranger halting");
    went(w.set_halted(6, &operator, true), "halt");
    refuses(w.buy(6, coin, SOL, 1_000_000), VaultError::Halted, "a buy while halted");
    went(w.sell(6, coin, 500_000, SOL / 2), "a sell while halted");
    went(w.set_halted(6, &operator, false), "unhalt");

    // The admin's pause: nothing trades at all.
    w.set_paused(true);
    refuses(w.buy(6, coin, SOL, 1_000_000), VaultError::Paused, "a buy while paused");
    refuses(w.sell(6, coin, 100_000, SOL / 10), VaultError::Paused, "a sell while paused");
    refuses(w.spawn(7), VaultError::Paused, "a spawn while paused");
    w.set_paused(false);
    went(w.buy(6, coin, SOL, 1_000_000), "a buy after the pause");
}

#[test]
fn lamports_parked_on_the_wrapped_address_cannot_jam_an_agent() {
    // Anyone can send SOL to the address the wrapped account is opened at. It
    // must not stop the agent trading, and it ends up in the vault.
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (vault, wsol) = (w.vault(6), w.wsol(6));
    let payer = w.payer.insecure_clone();
    let parked = SOL / 100;
    went(w.send(&[system_instruction::transfer(&payer.pubkey(), &wsol, parked)], &[]), "parking lamports");

    went(w.buy(6, coin, SOL, 1_000_000), "a buy with lamports parked");
    assert_eq!(w.lamports(&vault), 10 * SOL + parked - SOL - FEE_ON_ONE_SOL);
    assert_eq!(w.lamports(&wsol), 0);

    went(w.send(&[system_instruction::transfer(&payer.pubkey(), &wsol, 3 * SOL)], &[]), "parking more than the clip");
    let had = w.lamports(&vault);
    went(w.sell(6, coin, 400_000, SOL / 2), "a sell with lamports parked");
    let away = w.lamports(&w.bank(6)) + w.lamports(&w.pot()) - w.rent(0);
    assert_eq!(w.lamports(&vault) + away, had + 3 * SOL + SOL / 2 - (SOL / 2) * FEE_BPS / 10_000);
}

#[test]
fn coins_can_leave_only_to_the_operator() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    let (operator, stranger) = (w.operator.insecure_clone(), w.stranger.insecure_clone());
    let theirs = w.new_token_account(TOKEN, w.coin_mint, operator.pubkey(), 0);
    let not_theirs = w.new_token_account(TOKEN, w.coin_mint, stranger.pubkey(), 0);

    let take = |w: &mut World, by: &Keypair, to: Pubkey, amount: u64| {
        let ix = Instruction {
            program_id: w.program,
            accounts: pit_vault::accounts::WithdrawToken { agent: w.agent(6), vault: w.vault(6), operator: by.pubkey(), from: coin, to, mint: w.coin_mint, position: Some(w.position(6)), token_program: TOKEN }.to_account_metas(None),
            data: pit_vault::instruction::WithdrawToken { amount }.data(),
        };
        w.send(&[ix], &[by])
    };
    refuses(take(&mut w, &stranger, not_theirs, 1_000), VaultError::NotOperator, "a stranger taking coins");
    refuses(take(&mut w, &operator, not_theirs, 1_000), VaultError::NotOperatorAccount, "the operator sending coins to someone else");
    refuses(take(&mut w, &operator, theirs, 1_000_001), VaultError::InsufficientFunds, "more coins than are there");
    let cost = w.position_cost(6).unwrap();
    assert_eq!(cost, SOL + FEE_ON_ONE_SOL);
    went(take(&mut w, &operator, theirs, 250_000), "the operator taking coins");
    assert_eq!((w.tokens(&coin), w.tokens(&theirs)), (750_000, 250_000));
    // A quarter of the coins left with a quarter of their cost. No profit, no loss.
    assert_eq!(w.position_cost(6), Some(cost - cost / 4));
    assert_eq!(w.agent_state(6).realized, 0);

    // Pausing the floor does not trap them either.
    w.set_paused(true);
    went(take(&mut w, &operator, theirs, 750_000), "taking the rest while paused");
    assert_eq!((w.tokens(&coin), w.tokens(&theirs), w.tokens(&not_theirs)), (0, 1_000_000, 0));
}

#[test]
fn an_empty_coin_account_is_closed_and_its_rent_returned() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (executor, stranger) = (w.executor.insecure_clone(), w.stranger.insecure_clone());
    let close = |w: &mut World, by: &Keypair| {
        let ix = Instruction {
            program_id: w.program,
            accounts: pit_vault::accounts::CloseToken { config: w.config(), agent: w.agent(6), vault: w.vault(6), account: coin, authority: by.pubkey(), token_program: TOKEN }.to_account_metas(None),
            data: pit_vault::instruction::CloseToken {}.data(),
        };
        w.send(&[ix], &[by])
    };
    refuses(close(&mut w, &stranger), VaultError::NotAllowed, "a stranger closing it");
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    refuses(close(&mut w, &executor), VaultError::NotEmpty, "closing it with coins inside");
    went(w.sell(6, coin, 1_000_000, SOL), "sell it all");
    let before = w.lamports(&executor.pubkey());
    went(close(&mut w, &executor), "closing it empty");
    assert_eq!(w.lamports(&executor.pubkey()), before + w.rent(ACCOUNT_LEN));
    assert!(w.svm.get_account(&coin).map_or(true, |a| a.lamports == 0));
}

#[test]
fn clearing_fees_split_three_ways() {
    let mut w = World::new();
    let coin = w.funded(6, 400 * SOL);
    let collect = |w: &mut World, treasury: Pubkey| {
        let ix = Instruction {
            program_id: w.program,
            accounts: pit_vault::accounts::CollectFees { config: w.config(), fees: w.fees(), gains_pot: w.pot(), treasury, burner: w.burner, ecosystem: w.ecosystem, system_program: system_program::id() }.to_account_metas(None),
            data: pit_vault::instruction::CollectFees {}.data(),
        };
        w.send(&[ix], &[])
    };

    let treasury = w.treasury;
    went(w.buy(6, coin, SOL, 1_000_000), "a small buy");
    refuses(collect(&mut w, treasury), VaultError::NothingToCollect, "collecting too little");

    went(w.buy(6, coin, 40 * SOL, 1_000_000), "a large buy");
    let collected = w.lamports(&w.fees()) - w.rent(0);
    assert_eq!(collected, 41 * FEE_ON_ONE_SOL);
    assert!(collected >= MIN_COLLECT);
    let wrong = w.stranger.pubkey();
    refuses(collect(&mut w, wrong), VaultError::WrongDestination, "collecting to some other wallet");

    went(collect(&mut w, treasury), "collect");
    let (t, b, e) = (w.lamports(&w.treasury), w.lamports(&w.burner), w.lamports(&w.ecosystem));
    assert_eq!((t, b), (collected * 60 / 100, collected * 25 / 100));
    assert_eq!(t + b + e, collected, "every lamport went somewhere");
    assert_eq!(w.lamports(&w.fees()), w.rent(0));
}

#[test]
fn the_terms_have_ceilings_and_the_admin_changes_hands_in_two_steps() {
    let mut w = World::new();
    let (admin, stranger) = (w.admin.insecure_clone(), w.stranger.insecure_clone());

    refuses(w.propose(&stranger, w.terms()), VaultError::NotAdmin, "a stranger announcing terms");
    refuses(w.propose(&admin, Terms { clearing_fee_bps: 101, ..w.terms() }), VaultError::FeeTooHigh, "a clearing fee over 1%");
    refuses(w.propose(&admin, Terms { max_buy_bps: 5_001, ..w.terms() }), VaultError::BadTerms, "a clip over half the vault");
    refuses(w.propose(&admin, Terms { treasury_bps: 8_000, burn_bps: 2_001, ..w.terms() }), VaultError::BadTerms, "a split over the whole");
    refuses(w.propose(&admin, Terms { executor: admin.pubkey(), ..w.terms() }), VaultError::RolesNotSeparate, "the admin as its own trading key");
    refuses(w.propose(&admin, Terms { router: pit_vault::ID, ..w.terms() }), VaultError::BadTerms, "the vault as its own router");
    refuses(w.propose(&admin, Terms { payout_bps: 2_001, ..w.terms() }), VaultError::BadTerms, "an operator share over 20%");
    refuses(w.propose(&admin, Terms { profit_fee_bps: 501, ..w.terms() }), VaultError::FeeTooHigh, "a profit fee over 5%");
    went(w.propose(&admin, Terms { clearing_fee_bps: 100, ..w.terms() }), "a fee at the ceiling");
    w.warp(DAY);
    went(w.apply(&admin), "putting it into effect a day later");
    assert_eq!(w.config_state().terms.clearing_fee_bps, 100);

    let next = Keypair::new();
    let propose = Instruction {
        program_id: w.program,
        accounts: pit_vault::accounts::AdminOnly { config: w.config(), admin: admin.pubkey() }.to_account_metas(None),
        data: pit_vault::instruction::ProposeAdmin { next: next.pubkey() }.data(),
    };
    went(w.send(&[propose], &[&admin]), "propose");
    assert_eq!(w.config_state().admin, admin.pubkey(), "nothing changes until the new key accepts");
    let accept = |w: &mut World, by: &Keypair| {
        let ix = Instruction { program_id: w.program, accounts: pit_vault::accounts::AcceptAdmin { config: w.config(), next: by.pubkey() }.to_account_metas(None), data: pit_vault::instruction::AcceptAdmin {}.data() };
        w.send(&[ix], &[by])
    };
    refuses(accept(&mut w, &stranger), VaultError::NotAdmin, "a stranger accepting");
    went(accept(&mut w, &next), "the proposed key accepting");
    assert_eq!(w.config_state().admin, next.pubkey());
    refuses(w.propose(&admin, w.terms()), VaultError::NotAdmin, "the old admin announcing terms");
}

#[test]
fn profit_is_counted_to_the_lamport_across_partial_sells() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    let cost = SOL + FEE_ON_ONE_SOL;
    assert_eq!(w.position_cost(6), Some(cost), "the fee on the way in is part of what it cost");
    assert_eq!(w.agent_state(6).realized, 0, "a buy realises nothing");

    // Four tenths of the coins go for 0.6 SOL, taking four tenths of the cost with them.
    let proceeds = SOL * 6 / 10;
    went(w.sell(6, coin, 400_000, proceeds), "partial sell");
    let first = (proceeds - proceeds * FEE_BPS / 10_000) as i64 - (cost * 4 / 10) as i64;
    assert_eq!(w.agent_state(6).realized, first);
    assert_eq!(w.position_cost(6), Some(cost - cost * 4 / 10));

    // The rest goes for 0.3 SOL: a loss on that part, and the record is done.
    let proceeds = SOL * 3 / 10;
    went(w.sell(6, coin, 600_000, proceeds), "selling the rest");
    let second = (proceeds - proceeds * FEE_BPS / 10_000) as i64 - (cost - cost * 4 / 10) as i64;
    assert_eq!(w.agent_state(6).realized, first + second);
    assert_eq!(w.position_cost(6), None, "sold out, the record is closed");

    went(w.buy(6, coin, 2 * SOL, 1_000_000), "buying it again");
    assert_eq!(w.position_cost(6), Some(2 * cost), "a fresh record, not the old one");
}

#[test]
fn profit_goes_to_the_bank_and_a_fifth_of_it_to_the_operator_each_day() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (vault, bank, operator, stranger) = (w.vault(6), w.bank(6), w.operator.pubkey(), w.stranger.pubkey());

    // One SOL in, one and a half out.
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    assert_eq!(w.lamports(&bank), 0, "a buy banks nothing");
    went(w.sell(6, coin, 1_000_000, SOL * 3 / 2), "sell");
    let profit = SOL / 2 - FEE_ON_ONE_SOL - FEE_ON_ONE_SOL * 3 / 2;
    let a = w.agent_state(6);
    assert_eq!((a.realized, a.banked_mark, a.unshared), (profit as i64, profit as i64, profit));
    assert_eq!(w.lamports(&vault), 10 * SOL, "the wallet is back to exactly what was put in");
    assert_eq!(w.lamports(&bank), profit - profit / 100, "the profit is in the bank");
    assert_eq!(w.lamports(&w.pot()) - w.rent(0), profit / 100, "less the pit's hundredth, on its way to the treasury");

    refuses(w.payout(6, operator), VaultError::TooSoon, "a payout inside the first day");
    w.warp(DAY);
    refuses(w.payout(6, stranger), VaultError::NotOperator, "a payout to someone else's wallet");

    let had = w.lamports(&operator);
    went(w.payout(6, operator), "payout");
    assert_eq!(w.lamports(&operator), had + profit / 5, "a fifth of the profit, to the wallet that spawned it");
    assert_eq!(w.lamports(&bank), profit * 79 / 100, "the rest stays in the bank");
    assert_eq!(w.lamports(&vault), 10 * SOL, "and the capital is not touched");
    let a = w.agent_state(6);
    assert_eq!((a.paid_out, a.unshared), (profit / 5, 0));

    refuses(w.payout(6, operator), VaultError::TooSoon, "a second payout the same day");
    w.warp(DAY);
    refuses(w.payout(6, operator), VaultError::NothingToPay, "a day with no new profit");
}

#[test]
fn capital_is_never_banked_or_paid_out_and_a_loss_is_made_back_first() {
    let mut w = World::new();
    let coin = w.funded(6, 100 * SOL);
    let (bank, operator) = (w.bank(6), w.operator.pubkey());
    w.warp(DAY);
    // A wallet full of the operator's own money and no trades: nothing to share.
    refuses(w.payout(6, operator), VaultError::NothingToPay, "paying out capital");

    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, SOL * 7 / 10), "selling at a loss");
    assert!(w.agent_state(6).realized < 0);
    assert_eq!(w.lamports(&bank), 0, "a loss banks nothing");

    // A win smaller than the loss is still not profit.
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, SOL * 12 / 10), "a small win");
    assert!(w.agent_state(6).realized < 0);
    assert_eq!(w.lamports(&bank), 0, "a win that only claws back a loss banks nothing");
    refuses(w.payout(6, operator), VaultError::NothingToPay, "a payout before the loss is made back");

    // Back above water: only what is above it goes to the bank and is shared.
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, 2 * SOL), "a real win");
    let above = w.agent_state(6).realized as u64;
    assert_eq!(w.lamports(&bank), above - above / 100);
    let had = w.lamports(&operator);
    went(w.payout(6, operator), "payout");
    assert_eq!(w.lamports(&operator), had + above / 5);
}

#[test]
fn the_rest_of_the_bank_is_the_operators_to_take() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (bank, operator, stranger) = (w.bank(6), w.operator.insecure_clone(), w.stranger.insecure_clone());
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, SOL * 3 / 2), "sell");
    let profit = SOL / 2 - FEE_ON_ONE_SOL - FEE_ON_ONE_SOL * 3 / 2;
    let due = profit / 5;

    refuses(w.withdraw_bank(6, &stranger, SOL / 100), VaultError::NotOperator, "a stranger emptying the bank");
    // Everything but the share due at the next payout, and the account's rent, can come out now.
    let free = profit - profit / 100 - due - w.rent(0);
    refuses(w.withdraw_bank(6, &operator, free + 1), VaultError::BankHoldsNextPayout, "taking the next payout early");
    let had = w.lamports(&operator.pubkey());
    went(w.withdraw_bank(6, &operator, u64::MAX), "taking the rest of the bank");
    assert_eq!(w.lamports(&operator.pubkey()), had + free);
    assert_eq!(w.lamports(&bank), w.rent(0) + due);

    // The payout still arrives in full the next day, even while the floor is paused.
    w.set_paused(true);
    w.warp(DAY);
    let had = w.lamports(&operator.pubkey());
    went(w.payout(6, operator.pubkey()), "payout");
    assert_eq!(w.lamports(&operator.pubkey()), had + due);
    assert_eq!(w.lamports(&bank), w.rent(0));
}

#[test]
fn a_profit_too_small_to_open_the_bank_waits_for_the_next_one() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (vault, bank, operator) = (w.vault(6), w.bank(6), w.operator.pubkey());
    // Sold for a hair over cost: about half a thousandth of a SOL of profit,
    // less than the rent an account needs to exist.
    let out = 1_010_600_000;
    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, out), "a tiny win");
    let tiny = (out - out * FEE_BPS / 10_000 - SOL - FEE_ON_ONE_SOL) as i64;
    assert!(tiny > 0 && (tiny as u64) < w.rent(0));
    let a = w.agent_state(6);
    assert_eq!((a.realized, a.banked_mark), (tiny, 0), "counted, not yet banked");
    assert_eq!(w.lamports(&bank), 0);
    assert_eq!(w.lamports(&vault), 10 * SOL + tiny as u64, "it waits in the wallet");

    went(w.buy(6, coin, SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, out), "another");
    let both = 2 * tiny as u64;
    assert_eq!(w.lamports(&bank), both - both / 100, "the two go over together");
    assert_eq!(w.lamports(&vault), 10 * SOL);

    // The bank cannot pay below its own rent. It pays what it can and owes the rest.
    w.warp(DAY);
    let can = both - both / 100 - w.rent(0);
    assert!(can < both / 5);
    let had = w.lamports(&operator);
    went(w.payout(6, operator), "a payout from a bank that is nearly all rent");
    assert_eq!(w.lamports(&operator), had + can);
    assert_eq!(w.agent_state(6).unshared, both - can * 5, "what it could not pay is still owed");
}

#[test]
fn the_cut_of_gains_goes_whole_to_the_burn() {
    let mut w = World::new();
    let coin = w.funded(6, 400 * SOL);
    went(w.buy(6, coin, 40 * SOL, 1_000_000), "buy");
    went(w.sell(6, coin, 1_000_000, 60 * SOL), "sell");
    let clearing = 100 * FEE_ON_ONE_SOL;
    let profit = 20 * SOL - clearing;
    let cut = profit / 100;
    assert_eq!(w.lamports(&w.pot()) - w.rent(0), cut);

    let ix = Instruction {
        program_id: w.program,
        accounts: pit_vault::accounts::CollectFees { config: w.config(), fees: w.fees(), gains_pot: w.pot(), treasury: w.treasury, burner: w.burner, ecosystem: w.ecosystem, system_program: system_program::id() }.to_account_metas(None),
        data: pit_vault::instruction::CollectFees {}.data(),
    };
    went(w.send(&[ix], &[]), "collect");
    assert_eq!(w.lamports(&w.treasury), clearing * 60 / 100, "its cut of the clearing fees and nothing of the gains");
    assert_eq!(w.lamports(&w.burner), clearing * 25 / 100 + cut, "its cut of the clearing fees, and all of the cut of gains");
    assert_eq!(w.lamports(&w.ecosystem), clearing * 15 / 100);
    assert_eq!((w.lamports(&w.fees()), w.lamports(&w.pot())), (w.rent(0), w.rent(0)));
}

#[test]
fn a_holder_pays_half_the_clearing_fee_and_half_the_cut_of_gains() {
    let mut w = World::new();
    let registrar = w.registrar.insecure_clone();
    // Two agents making the same trade, one of them in the holder class.
    let plain = w.funded(6, 400 * SOL);
    let operator = w.operator.insecure_clone();
    went(w.spawn_as(7, &operator, w.operator_pit, &registrar), "a second spawn");
    let payer = w.payer.insecure_clone();
    went(w.send(&[system_instruction::transfer(&payer.pubkey(), &w.vault(7), 400 * SOL)], &[]), "fund");
    let held = w.new_token_account(w.coin_program, w.coin_mint, w.vault(7), 0);
    assert!(w.tokens(&w.operator_pit) >= HOLDING, "the operator still holds enough after two burns");
    went(w.grant_holder(7, HOLDING, w.operator_pit, &registrar), "putting it in the holder class");
    assert!(w.agent_state(7).holder && !w.agent_state(6).holder);

    let (fees, pot) = (w.fees(), w.pot());
    let (fees_0, pot_0) = (w.lamports(&fees), w.lamports(&pot));
    went(w.buy(6, plain, 40 * SOL, 1_000_000), "the ordinary agent buys");
    went(w.sell(6, plain, 1_000_000, 60 * SOL), "and sells");
    let (full_fee, full_cut) = (w.lamports(&fees) - fees_0, w.lamports(&pot) - pot_0);
    assert_eq!(full_fee, 100 * FEE_ON_ONE_SOL);
    assert_eq!(full_cut, (20 * SOL - full_fee) / 100);

    let (fees_1, pot_1) = (w.lamports(&fees), w.lamports(&pot));
    went(w.buy(7, held, 40 * SOL, 1_000_000), "the holder's agent buys");
    went(w.sell(7, held, 1_000_000, 60 * SOL), "and sells");
    let (half_fee, half_cut) = (w.lamports(&fees) - fees_1, w.lamports(&pot) - pot_1);
    assert_eq!(half_fee, full_fee / 2, "half the clearing fee, both ways");
    // It paid less in fees, so it made a little more, and half of the cut is taken on that.
    assert_eq!(half_cut, (20 * SOL - half_fee) / 100 / 2, "half the cut of gains");
    assert_eq!(w.agent_state(7).realized, (20 * SOL - half_fee) as i64);
    assert_eq!(w.lamports(&w.bank(7)), 20 * SOL - half_fee - half_cut, "and the rest is in its bank");

    // Taken out of the class, it pays in full again from the next fill.
    went(w.revoke_holder(7, &registrar), "taking it out of the holder class");
    assert!(!w.agent_state(7).holder);
    let fees_2 = w.lamports(&fees);
    went(w.buy(7, held, 40 * SOL, 1_000_000), "buying again");
    assert_eq!(w.lamports(&fees) - fees_2, 40 * FEE_ON_ONE_SOL, "the full fee");
}

#[test]
fn half_fees_are_only_for_an_operator_who_really_holds_the_pit() {
    let mut w = World::new();
    w.funded(6, 10 * SOL);
    let (registrar, stranger) = (w.registrar.insecure_clone(), w.stranger.insecure_clone());

    refuses(w.grant_holder(6, HOLDING, w.operator_pit, &stranger), VaultError::NotRegistrar, "a stranger handing out half fees");
    refuses(w.revoke_holder(6, &stranger), VaultError::NotRegistrar, "a stranger taking them away");
    refuses(w.grant_holder(6, HOLDER_MIN - 1, w.operator_pit, &registrar), VaultError::HoldingTooSmall, "the registrar pricing it under the least the terms allow");

    // Somebody else's $PIT does not count, however much of it there is.
    let theirs = w.new_token_account(TOKEN_2022, w.pit_mint, stranger.pubkey(), 0);
    w.mint_to(TOKEN_2022, w.pit_mint, theirs, 10 * HOLDING);
    refuses(w.grant_holder(6, HOLDING, theirs, &registrar), VaultError::NotOperatorAccount, "pointing at a stranger's $PIT");

    // Nor does some other token the operator holds.
    let other = w.new_mint(TOKEN_2022, 6);
    let lookalike = w.new_token_account(TOKEN_2022, other, w.operator.pubkey(), 0);
    w.mint_to(TOKEN_2022, other, lookalike, 10 * HOLDING);
    refuses(w.grant_holder(6, HOLDING, lookalike, &registrar), VaultError::WrongMint, "pointing at another token");

    // Nor does too little of the real thing.
    let held = w.tokens(&w.operator_pit);
    refuses(w.grant_holder(6, held + 1, w.operator_pit, &registrar), VaultError::HoldingTooSmall, "holding one unit too few");
    assert!(!w.agent_state(6).holder, "nothing above put it in the class");
    went(w.grant_holder(6, held, w.operator_pit, &registrar), "holding exactly enough");
    assert!(w.agent_state(6).holder);

    // With the least set to nothing, the class is shut to everyone.
    let admin = w.admin.insecure_clone();
    went(w.propose(&admin, Terms { holder_min: 0, ..w.terms() }), "announcing the class shut");
    w.warp(DAY);
    went(w.apply(&admin), "shutting it");
    went(w.revoke_holder(6, &registrar), "taking it out");
    refuses(w.grant_holder(6, held, w.operator_pit, &registrar), VaultError::HolderClosed, "putting it back while the class is shut");
}

#[test]
fn the_terms_cannot_change_overnight() {
    let mut w = World::new();
    let coin = w.funded(6, 10 * SOL);
    let (admin, stranger, operator) = (w.admin.insecure_clone(), w.stranger.insecure_clone(), w.operator.insecure_clone());
    let impostor = Keypair::new();
    w.svm.airdrop(&impostor.pubkey(), 10 * SOL).unwrap();
    let router = w.router;

    refuses(w.apply(&admin), VaultError::NoTermsPending, "putting into effect terms nobody announced");
    // The admin key, stolen or not, names a new trading key.
    went(w.propose(&admin, Terms { executor: impostor.pubkey(), ..w.terms() }), "announcing a new trading key");
    assert_eq!(w.config_state().terms.executor, w.executor.pubkey(), "nothing has changed yet");
    refuses(w.apply(&admin), VaultError::TermsNotReady, "putting it into effect the same day");
    refuses(w.apply(&stranger), VaultError::NotAdmin, "a stranger putting it into effect");
    refuses(w.swap_with(6, true, coin, SOL, 1, HONEST, SOL, 1_000_000, None, &impostor, router), VaultError::NotExecutor, "the new key trading before its day is up");
    went(w.buy(6, coin, SOL, 1_000_000), "the old key still trading");

    // An operator who does not like what was announced has the day to leave.
    w.warp(DAY - 60);
    refuses(w.apply(&admin), VaultError::TermsNotReady, "a minute early");
    went(w.withdraw(6, &operator, u64::MAX), "the operator taking their SOL out in time");

    w.warp(60);
    went(w.apply(&admin), "a full day on");
    assert_eq!(w.config_state().terms.executor, impostor.pubkey());
    refuses(w.buy(6, coin, SOL, 1_000_000), VaultError::NotExecutor, "the old key trading after the change");
    refuses(w.apply(&admin), VaultError::NoTermsPending, "applying the same terms twice");

    // An announcement can be taken back, and then there is nothing to apply.
    went(w.propose(&admin, Terms { max_buy_bps: 100, ..w.terms() }), "announcing something else");
    refuses(w.cancel(&stranger), VaultError::NotAdmin, "a stranger cancelling");
    went(w.cancel(&admin), "cancelling");
    w.warp(DAY);
    refuses(w.apply(&admin), VaultError::NoTermsPending, "applying what was cancelled");
}
