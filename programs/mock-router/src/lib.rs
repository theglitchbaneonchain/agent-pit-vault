//! Test only. Stands in for the swap router.
//!
//! In its honest mode it does what a swap does: takes the input from the
//! caller's source account, on the caller's signature, and pays the output
//! from its own pool. The other modes do what a hostile or buggy router could
//! try with a vault's signature, so the vault can be shown to refuse each one.
//!
//! Data: mode u8, take u64, give u64.
//! Accounts: source, destination, authority (signer), pool in, pool out,
//! pool authority, token program in, token program out, then one spare.

use anchor_lang::solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
};

pub const HONEST: u8 = 0;
/// Pays the output to the spare account instead of the caller's.
pub const PAY_ELSEWHERE: u8 = 1;
/// Leaves a delegate on the destination account.
pub const DELEGATE_DESTINATION: u8 = 3;
/// Leaves a delegate on the source account.
pub const DELEGATE_SOURCE: u8 = 4;
/// Hands ownership of the destination account to the spare.
pub const REOWN_DESTINATION: u8 = 5;
/// Assigns the caller's own wallet to this program. The spare is the system program.
pub const SEIZE_WALLET: u8 = 6;

#[cfg(not(feature = "no-entrypoint"))]
anchor_lang::solana_program::entrypoint!(process);

fn amount(tag: u8, n: u64) -> Vec<u8> {
    let mut d = vec![tag];
    d.extend_from_slice(&n.to_le_bytes());
    d
}

fn transfer(program: &Pubkey, from: &Pubkey, to: &Pubkey, authority: &Pubkey, n: u64) -> Instruction {
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*from, false), AccountMeta::new(*to, false), AccountMeta::new_readonly(*authority, true)], data: amount(3, n) }
}

fn approve(program: &Pubkey, account: &Pubkey, delegate: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*account, false), AccountMeta::new_readonly(*delegate, false), AccountMeta::new_readonly(*owner, true)], data: amount(4, u64::MAX) }
}

fn reown(program: &Pubkey, account: &Pubkey, owner: &Pubkey, next: &Pubkey) -> Instruction {
    let mut data = vec![6, 2, 1];
    data.extend_from_slice(next.as_ref());
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*account, false), AccountMeta::new_readonly(*owner, true)], data }
}

pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [source, destination, authority, pool_in, pool_out, pool_authority, token_in, token_out, rest @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if data.len() != 17 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mode = data[0];
    let take = u64::from_le_bytes(data[1..9].try_into().unwrap());
    let give = u64::from_le_bytes(data[9..17].try_into().unwrap());
    let spare = rest.first();
    let (pool, bump) = Pubkey::find_program_address(&[b"pool"], program_id);

    if take > 0 {
        invoke(&transfer(token_in.key, source.key, pool_in.key, authority.key, take), &[source.clone(), pool_in.clone(), authority.clone()])?;
    }
    if give > 0 {
        let to = if mode == PAY_ELSEWHERE { spare.ok_or(ProgramError::NotEnoughAccountKeys)? } else { destination };
        invoke_signed(&transfer(token_out.key, pool_out.key, to.key, &pool, give), &[pool_out.clone(), to.clone(), pool_authority.clone()], &[&[b"pool", &[bump]]])?;
    }

    match mode {
        DELEGATE_DESTINATION => invoke(&approve(token_out.key, destination.key, &pool, authority.key), &[destination.clone(), pool_authority.clone(), authority.clone()])?,
        DELEGATE_SOURCE => invoke(&approve(token_in.key, source.key, &pool, authority.key), &[source.clone(), pool_authority.clone(), authority.clone()])?,
        REOWN_DESTINATION => invoke(&reown(token_out.key, destination.key, authority.key, &pool), &[destination.clone(), authority.clone()])?,
        SEIZE_WALLET => {
            let system = spare.ok_or(ProgramError::NotEnoughAccountKeys)?;
            let mut data = vec![1, 0, 0, 0];
            data.extend_from_slice(program_id.as_ref());
            invoke(&Instruction { program_id: *system.key, accounts: vec![AccountMeta::new(*authority.key, true)], data }, &[authority.clone()])?;
        }
        _ => {}
    }
    Ok(())
}
