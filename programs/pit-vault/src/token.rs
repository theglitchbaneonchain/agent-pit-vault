//! The handful of SPL Token pieces the vault needs, written out by hand.
//!
//! Both token programs share these instruction encodings and the first 165
//! bytes of an account, which is everything read here.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

use crate::VaultError;

pub const TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub const ACCOUNT_LEN: usize = 165;
const MINT_LEN: usize = 82;
const STATE_INITIALIZED: u8 = 1;
const KIND_ACCOUNT: u8 = 2;

pub fn is_token_program(key: &Pubkey) -> bool {
    *key == TOKEN || *key == TOKEN_2022
}

/// What the vault needs to know about a token account.
pub struct TokenView {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub has_delegate: bool,
    pub has_close_authority: bool,
}

fn key_at(data: &[u8], at: usize) -> Pubkey {
    let mut k = [0u8; 32];
    k.copy_from_slice(&data[at..at + 32]);
    Pubkey::new_from_array(k)
}

/// Reads a token account, or says it is not one. A frozen account is refused:
/// nothing can be moved out of it, so it is not something to trade into.
pub fn view(ai: &AccountInfo) -> Option<TokenView> {
    if !is_token_program(ai.owner) {
        return None;
    }
    let d = ai.try_borrow_data().ok()?;
    let is_account = if *ai.owner == TOKEN { d.len() == ACCOUNT_LEN } else { d.len() == ACCOUNT_LEN || (d.len() > ACCOUNT_LEN && d[ACCOUNT_LEN] == KIND_ACCOUNT) };
    if !is_account || d[108] != STATE_INITIALIZED {
        return None;
    }
    let mut amount = [0u8; 8];
    amount.copy_from_slice(&d[64..72]);
    Some(TokenView {
        mint: key_at(&d, 0),
        owner: key_at(&d, 32),
        amount: u64::from_le_bytes(amount),
        has_delegate: d[72..76] != [0, 0, 0, 0],
        has_close_authority: d[129..133] != [0, 0, 0, 0],
    })
}

pub fn must_view(ai: &AccountInfo) -> Result<TokenView> {
    view(ai).ok_or_else(|| error!(VaultError::NotTokenAccount))
}

/// A mint's decimals, checked against the token program that owns it.
pub fn decimals(mint: &AccountInfo) -> Result<u8> {
    require!(is_token_program(mint.owner), VaultError::NotMint);
    let d = mint.try_borrow_data()?;
    require!(d.len() >= MINT_LEN && d[45] == 1, VaultError::NotMint);
    Ok(d[44])
}

fn with_amount(tag: u8, amount: u64, decimals: Option<u8>) -> Vec<u8> {
    let mut data = Vec::with_capacity(10);
    data.push(tag);
    data.extend_from_slice(&amount.to_le_bytes());
    if let Some(d) = decimals {
        data.push(d);
    }
    data
}

pub fn initialize_account3(program: &Pubkey, account: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> Instruction {
    let mut data = Vec::with_capacity(33);
    data.push(18);
    data.extend_from_slice(owner.as_ref());
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*account, false), AccountMeta::new_readonly(*mint, false)], data }
}

pub fn close_account(program: &Pubkey, account: &Pubkey, to: &Pubkey, owner: &Pubkey) -> Instruction {
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*account, false), AccountMeta::new(*to, false), AccountMeta::new_readonly(*owner, true)], data: vec![9] }
}

pub fn burn_checked(program: &Pubkey, account: &Pubkey, mint: &Pubkey, authority: &Pubkey, amount: u64, decimals: u8) -> Instruction {
    Instruction { program_id: *program, accounts: vec![AccountMeta::new(*account, false), AccountMeta::new(*mint, false), AccountMeta::new_readonly(*authority, true)], data: with_amount(15, amount, Some(decimals)) }
}

pub fn transfer_checked(program: &Pubkey, from: &Pubkey, mint: &Pubkey, to: &Pubkey, authority: &Pubkey, amount: u64, decimals: u8) -> Instruction {
    Instruction {
        program_id: *program,
        accounts: vec![AccountMeta::new(*from, false), AccountMeta::new_readonly(*mint, false), AccountMeta::new(*to, false), AccountMeta::new_readonly(*authority, true)],
        data: with_amount(12, amount, Some(decimals)),
    }
}
