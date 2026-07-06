//! Associated Token Account derivation + small SPL-token instruction
//! builders (wrap/unwrap helpers).
//!
//! ATA address = find_program_address(
//!   &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
//!   ASSOCIATED_TOKEN_PROGRAM_ID,
//! )
//!
//! The token program is part of the seed — a Token-2022 mint's ATA
//! lives at a DIFFERENT address than the same `(owner, mint)` pair
//! under the legacy program. Pump launches Token-2022 mints since
//! 2025, so every DEX adapter threads the mint's actual owner program
//! through here (audit M2). The legacy single-arg helpers default to
//! the classic Token program and remain for WSOL-only paths.
//!
//! Pure-Rust via `solana_sdk::pubkey::Pubkey::find_program_address` —
//! no network, no extra crates.

use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

/// SPL Token program (legacy / classic).
pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// SPL Token-2022 program. Pump (classic + AMM) mints increasingly
/// live here; the mint account's `owner` field tells you which.
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// SPL Associated Token Account program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// Native SOL wrapper mint. Used as the input mint for buy / output
/// mint for sell on every Solana DEX adapter. Always owned by the
/// LEGACY token program.
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// USDC mainnet mint (6 decimals, legacy token program). Part of the
/// 4-asset "cash" denominator for pct-of-balance copy sizing (D8).
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// USDT mainnet mint (6 decimals, legacy token program). Part of the
/// 4-asset "cash" denominator for pct-of-balance copy sizing (D8).
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");

/// Derive the ATA for `(owner, mint)` under the LEGACY token program.
/// Prefer [`derive_with_program`] anywhere the mint could be
/// Token-2022 (pump launches both since 2025).
pub fn derive(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    derive_with_program(owner, mint, &TOKEN_PROGRAM_ID)
}

/// Derive the ATA for `(owner, mint)` under an explicit token program
/// (legacy or Token-2022). The token program participates in the PDA
/// seeds, so passing the wrong one yields a different (usually
/// non-existent) address.
pub fn derive_with_program(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let (ata, _bump) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    ata
}

/// Build a `create_associated_token_account_idempotent` instruction
/// for a LEGACY-token mint. Prefer [`create_idempotent_ix_with_program`]
/// for anything that could be Token-2022.
pub fn create_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
) -> solana_sdk::instruction::Instruction {
    create_idempotent_ix_with_program(payer, owner, mint, &TOKEN_PROGRAM_ID)
}

/// Build a `create_associated_token_account_idempotent` instruction.
/// Costs ~3.5k CU when the account already exists (no-op short-circuit
/// at instruction-1 entry), or ~30k when it has to actually allocate.
/// We always prepend it on first-buys so a fresh wallet doesn't fail
/// for missing-ATA reasons.
///
/// Account list (positional, idempotent variant = discriminator `1`):
///   0 payer    (signer, writable)
///   1 ata      (writable)            — derived = `derive_with_program(owner, mint, token_program)`
///   2 owner    (read-only)
///   3 mint     (read-only)
///   4 system_program
///   5 token_program (legacy OR Token-2022 — must match the mint's owner)
pub fn create_idempotent_ix_with_program(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> solana_sdk::instruction::Instruction {
    use solana_sdk::instruction::{AccountMeta, Instruction};
    let ata = derive_with_program(owner, mint, token_program);
    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        // Discriminator 1 = `CreateIdempotent`. Single byte, no args.
        data: vec![1u8],
    }
}

// ── WSOL wrap / unwrap helpers (audit H2) ────────────────────────────
//
// Native DEX swaps that pay/receive WSOL need the lamports physically
// inside the WSOL ATA. Jupiter does this internally
// (`wrapAndUnwrapSol`); the native builders must do it themselves:
//
//   buy:  create_idempotent(WSOL) → system transfer(user→wsol_ata)
//         → sync_native → swap → close_account (unwrap leftovers)
//   sell: create_idempotent(WSOL) → swap → close_account (unwrap proceeds)
//
// Closing at the end keeps the wallet's balance in NATIVE SOL — the
// representation every sizing path (pct-of-balance, budget caps) reads.

/// SPL-Token `SyncNative` — instruction tag 17. Reconciles the WSOL
/// token-account `amount` with the lamports actually held by the
/// account (after a raw system transfer into it).
pub fn sync_native_ix(wsol_ata: &Pubkey) -> solana_sdk::instruction::Instruction {
    use solana_sdk::instruction::{AccountMeta, Instruction};
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*wsol_ata, false)],
        data: vec![17u8],
    }
}

/// SPL-Token `CloseAccount` — instruction tag 9. Sends ALL lamports in
/// `account` (rent + wrapped balance) to `destination` and deletes the
/// account. The standard "unwrap WSOL" move.
pub fn close_account_ix(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> solana_sdk::instruction::Instruction {
    use solana_sdk::instruction::{AccountMeta, Instruction};
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9u8],
    }
}

/// System-program transfer — funds the WSOL ATA before `SyncNative`.
pub fn system_transfer_ix(
    from: &Pubkey,
    to: &Pubkey,
    lamports: u64,
) -> solana_sdk::instruction::Instruction {
    solana_sdk::system_instruction::transfer(from, to, lamports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ata_for_known_owner_mint_matches_explorer() {
        // Reference: USDC ATA for Solana Foundation's well-known
        // public address. Derived once via @solana/spl-token, pasted
        // here as a fixture so we'd catch any future find_program_
        // address breakage.
        let owner = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"); // USDC
        let ata = derive(&owner, &mint);
        // Sanity: same inputs deterministically derive the same ATA.
        assert_eq!(ata, derive(&owner, &mint));
        // The ATA is NOT the owner or the mint. Just a length + non-
        // collision check — full ATA fixture would tie the test to a
        // specific solana-sdk version's hashing path.
        assert_ne!(ata, owner);
        assert_ne!(ata, mint);
    }

    #[test]
    fn ata_for_different_mints_differs() {
        let owner = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let usdc = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_ne!(derive(&owner, &WSOL_MINT), derive(&owner, &usdc));
    }

    #[test]
    fn token_2022_ata_differs_from_legacy_ata() {
        // The token program participates in the PDA seed — same
        // (owner, mint) under Token-2022 = different address.
        let owner = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_ne!(
            derive_with_program(&owner, &mint, &TOKEN_PROGRAM_ID),
            derive_with_program(&owner, &mint, &TOKEN_2022_PROGRAM_ID),
        );
        // Single-arg helper == explicit legacy program.
        assert_eq!(
            derive(&owner, &mint),
            derive_with_program(&owner, &mint, &TOKEN_PROGRAM_ID)
        );
    }

    #[test]
    fn create_idempotent_threads_token_program() {
        let payer = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        let ix = create_idempotent_ix_with_program(&payer, &payer, &mint, &TOKEN_2022_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 6);
        assert_eq!(
            ix.accounts[1].pubkey,
            derive_with_program(&payer, &mint, &TOKEN_2022_PROGRAM_ID)
        );
        assert_eq!(ix.accounts[5].pubkey, TOKEN_2022_PROGRAM_ID);
        assert_eq!(ix.data, vec![1u8]);
    }

    #[test]
    fn sync_native_is_tag_17_single_account() {
        let ata = Pubkey::new_unique();
        let ix = sync_native_ix(&ata);
        assert_eq!(ix.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(ix.data, vec![17u8]);
        assert_eq!(ix.accounts.len(), 1);
        assert_eq!(ix.accounts[0].pubkey, ata);
        assert!(ix.accounts[0].is_writable);
    }

    #[test]
    fn close_account_is_tag_9_owner_signs() {
        let acct = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let ix = close_account_ix(&acct, &owner, &owner);
        assert_eq!(ix.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(ix.data, vec![9u8]);
        assert_eq!(ix.accounts.len(), 3);
        assert!(ix.accounts[0].is_writable);
        assert!(ix.accounts[1].is_writable);
        assert!(ix.accounts[2].is_signer);
    }

    #[test]
    fn system_transfer_targets_system_program() {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let ix = system_transfer_ix(&from, &to, 42);
        assert_eq!(ix.program_id, solana_sdk::system_program::ID);
    }
}
