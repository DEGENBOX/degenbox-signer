//! PumpFun classic — native instruction encoder (v2 instructions).
//!
//! Rebuilt 2026-06-11 against the LIVE on-chain program (audit B1).
//! The pre-May-2025 12-account legacy layout this module used to emit
//! is rejected by the deployed program. The deployed LEGACY `buy`/
//! `sell` handlers additionally demand undeclared *remaining accounts*
//! (buyback fee recipient et al, error 6062 `BuybackFeeRecipientMissing`
//! when omitted) whose full protocol is not declared in the IDL — so
//! this encoder targets the **`buy_v2` / `sell_v2`** instructions
//! instead: the unified interface the on-chain Anchor IDL v0.1.0
//! declares COMPLETELY (27 / 26 mandatory accounts, every PDA seeded).
//! Pump's public docs recommend the v2 interface for all new
//! integrations; for SOL-paired coins none of the quote-side ATAs
//! need to exist, so no extra rent or wrap instructions are required.
//!
//! - Discriminators (`sha256("global:<ix>")[..8]`):
//!   `buy_v2 = 0xb817ee6167c5d33d` · `sell_v2 = 0x5df6823ce7e940b2`
//! - Args: `amount: u64 LE` + `sol_bound: u64 LE` (24-byte data, no
//!   `track_volume` arg on v2).
//! - Account order: see `build_buy_ix` / `build_sell_ix` — verified
//!   against the on-chain IDL and by live mainnet simulation (see
//!   `tests/live_dex_sim.rs`).
//!
//! PDAs introduced by the 2025/26 fee rework (all pinned against live
//! mainnet accounts in the tests below):
//!   creator_vault             = `["creator-vault", curve.creator]`
//!   global_volume_accumulator = `["global_volume_accumulator"]`
//!   user_volume_accumulator   = `["user_volume_accumulator", user]`
//!   sharing_config            = `["sharing-config", mint]` on the FEE
//!                               program (uninitialized for most coins
//!                               — passed anyway, Anchor-seed-checked)
//!   fee_config                = `["fee_config", PUMP_PROGRAM_ID]` on
//!                               the fee program `pfeeUxB6…`
//!
//! The mint's token program is threaded through every base-side ATA
//! derivation — pump launches **Token-2022** mints since 2025 (audit
//! M2); the caller fetches the mint account's `owner` and passes it in.
//! The quote side is WSOL (legacy token program) for every coin this
//! signer trades; non-SOL-quoted curves route to Jupiter upstream.
//!
//! No bonding-curve math here — the caller fetches the curve state via
//! RPC and supplies the slippage bounds. Pure encoder + PDA derivation,
//! audit-friendly.

use crate::dex::{ata, compute_budget};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;

/// PumpFun classic program id.
pub const PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

/// Pump fee program — computes the protocol fee split from the
/// `fee_config` PDA it owns. Appended to every buy/sell since the
/// 2025 fees rework.
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

/// Protocol fee recipient. The live `Global` account carries a
/// `fee_recipients: [Pubkey; 7]` rotation — any member is accepted by
/// the program. This one is `Global.fee_recipient` (the primary slot,
/// decoded live 2026-06-11) and was also observed on a live classic
/// buy (sig `HAUnBUBSnxKfgqrTqrLDhQmx7e8a149KNFg62U6e8E7FTyr9voqb2chdZpUQ3JWc6TSZeh9MiyiRzeX2M3x53kk`)
/// and every PumpSwap swap we sampled. The old `CebN5W…` constant is a
/// non-primary rotation member at best (audit B1).
pub const FEE_RECIPIENT: Pubkey = pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");

/// Buyback fee recipient — `Global.buyback_fee_recipients[0]` (decoded
/// live 2026-06-11; the program accepts any member of the 8-slot
/// array). Observed live on a PumpSwap buy
/// (sig `5Ls8BMWGf3JC1VF3hqn6uxjy4qYDk6c8j3WptJuRT5iYu234u3vKvyvdWgD2PeJP76MzGyWuymRanpfiJ1Hqyt3j`)
/// and sell. Owned by the fee program.
pub const BUYBACK_FEE_RECIPIENT: Pubkey = pubkey!("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");

/// WSOL mint — the quote side of every SOL-paired curve.
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// Anchor sighash discriminators (first 8 bytes of `sha256("global:<ix>")`).
pub const DISCRIM_BUY: [u8; 8] = [0xb8, 0x17, 0xee, 0x61, 0x67, 0xc5, 0xd3, 0x3d]; // buy_v2
pub const DISCRIM_SELL: [u8; 8] = [0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2]; // sell_v2

/// Derive the global config PDA. Seed = `"global"`.
pub fn global_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"global"], &PROGRAM_ID);
    pda
}

/// Derive the bonding-curve PDA for a given mint. Seed = `"bonding-curve" || mint`.
pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PROGRAM_ID);
    pda
}

/// Derive the event-authority PDA. Seed = `"__event_authority"` (double
/// underscore — Anchor's CPI-event-relay convention).
pub fn event_authority_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID);
    pda
}

/// Derive the creator-vault PDA — receives the coin creator's fee cut.
/// Seed = `"creator-vault" || creator` (creator from the BondingCurve
/// account, byte offset 49).
pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &PROGRAM_ID);
    pda
}

/// Derive the global volume accumulator PDA (buys only).
pub fn global_volume_accumulator_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"global_volume_accumulator"], &PROGRAM_ID);
    pda
}

/// Derive the per-user volume accumulator PDA (buys only).
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PROGRAM_ID);
    pda
}

/// Derive the fee-config PDA — owned by [`FEE_PROGRAM_ID`], seeded
/// with the pump program id: `["fee_config", PROGRAM_ID]`.
pub fn fee_config_pda() -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"fee_config", PROGRAM_ID.as_ref()], &FEE_PROGRAM_ID);
    pda
}

/// Derive the per-mint sharing-config PDA — `["sharing-config", mint]`
/// on the FEE program. Uninitialized for coins without creator-fee
/// sharing; the v2 handlers accept it uninitialized (seed-checked only).
pub fn sharing_config_pda(mint: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"sharing-config", mint.as_ref()], &FEE_PROGRAM_ID);
    pda
}

/// Accounts 0..=17 shared by `buy_v2` + `sell_v2` (IDL order).
/// Quote side is hard-pinned to WSOL/legacy — non-SOL-quoted curves
/// never reach this builder (routed to Jupiter upstream).
fn common_v2_accounts(
    user: &Pubkey,
    mint: &Pubkey,
    creator: &Pubkey,
    token_program: &Pubkey,
) -> Vec<AccountMeta> {
    let quote_tp = ata::TOKEN_PROGRAM_ID;
    let bonding_curve = bonding_curve_pda(mint);
    let creator_vault = creator_vault_pda(creator);
    vec![
        AccountMeta::new_readonly(global_pda(), false), // 0 global
        AccountMeta::new_readonly(*mint, false),        // 1 base_mint
        AccountMeta::new_readonly(WSOL_MINT, false),    // 2 quote_mint
        AccountMeta::new_readonly(*token_program, false), // 3 base_token_program
        AccountMeta::new_readonly(quote_tp, false),     // 4 quote_token_program
        AccountMeta::new_readonly(ata::ASSOCIATED_TOKEN_PROGRAM_ID, false), // 5
        AccountMeta::new(FEE_RECIPIENT, false),         // 6 fee_recipient
        AccountMeta::new(
            ata::derive_with_program(&FEE_RECIPIENT, &WSOL_MINT, &quote_tp),
            false,
        ), // 7 associated_quote_fee_recipient
        AccountMeta::new(BUYBACK_FEE_RECIPIENT, false), // 8 buyback_fee_recipient
        AccountMeta::new(
            ata::derive_with_program(&BUYBACK_FEE_RECIPIENT, &WSOL_MINT, &quote_tp),
            false,
        ), // 9 associated_quote_buyback_fee_recipient
        AccountMeta::new(bonding_curve, false),         // 10 bonding_curve
        AccountMeta::new(
            ata::derive_with_program(&bonding_curve, mint, token_program),
            false,
        ), // 11 associated_base_bonding_curve
        AccountMeta::new(
            ata::derive_with_program(&bonding_curve, &WSOL_MINT, &quote_tp),
            false,
        ), // 12 associated_quote_bonding_curve
        AccountMeta::new(*user, true),                  // 13 user (signer)
        AccountMeta::new(ata::derive_with_program(user, mint, token_program), false), // 14 associated_base_user
        AccountMeta::new(ata::derive_with_program(user, &WSOL_MINT, &quote_tp), false), // 15 associated_quote_user
        AccountMeta::new(creator_vault, false), // 16 creator_vault
        AccountMeta::new(
            ata::derive_with_program(&creator_vault, &WSOL_MINT, &quote_tp),
            false,
        ), // 17 associated_creator_vault
    ]
}

/// Tail shared by both v2 instructions after the volume-accumulator
/// block: fee_config · fee_program · system · event_authority · program.
fn v2_tail(accounts: &mut Vec<AccountMeta>) {
    accounts.push(AccountMeta::new_readonly(fee_config_pda(), false));
    accounts.push(AccountMeta::new_readonly(FEE_PROGRAM_ID, false));
    accounts.push(AccountMeta::new_readonly(
        solana_sdk::system_program::ID,
        false,
    ));
    accounts.push(AccountMeta::new_readonly(event_authority_pda(), false));
    accounts.push(AccountMeta::new_readonly(PROGRAM_ID, false));
}

/// Build a `buy_v2` instruction (27 accounts, on-chain IDL order).
///
/// * `user` — wallet that signs and pays SOL.
/// * `mint` — base token mint.
/// * `creator` — the coin creator from the BondingCurve account
///   (drives the `creator_vault` PDA).
/// * `token_program` — the mint account's owner (legacy SPL Token or
///   Token-2022).
/// * `amount` — base tokens the user wants (in raw token decimals).
/// * `max_sol_cost` — slippage upper-bound in lamports.
pub fn build_buy_ix(
    user: &Pubkey,
    mint: &Pubkey,
    creator: &Pubkey,
    token_program: &Pubkey,
    amount: u64,
    max_sol_cost: u64,
) -> Instruction {
    let quote_tp = ata::TOKEN_PROGRAM_ID;
    let mut accounts = common_v2_accounts(user, mint, creator, token_program);
    let uva = user_volume_accumulator_pda(user);
    accounts.push(AccountMeta::new_readonly(sharing_config_pda(mint), false)); // 18
    accounts.push(AccountMeta::new_readonly(
        global_volume_accumulator_pda(),
        false,
    )); // 19 (buys only)
    accounts.push(AccountMeta::new(uva, false)); // 20
    accounts.push(AccountMeta::new(
        ata::derive_with_program(&uva, &WSOL_MINT, &quote_tp),
        false,
    )); // 21 associated_user_volume_accumulator
    v2_tail(&mut accounts); // 22..=26
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: encode_args(DISCRIM_BUY, amount, max_sol_cost),
    }
}

/// Build a `sell_v2` instruction (26 accounts, on-chain IDL order).
///
/// * `user` — wallet that signs and receives SOL.
/// * `mint` — base token mint being sold.
/// * `creator` — coin creator from the BondingCurve account.
/// * `token_program` — the mint account's owner program.
/// * `amount` — base tokens to sell.
/// * `min_sol_output` — slippage lower-bound in lamports.
pub fn build_sell_ix(
    user: &Pubkey,
    mint: &Pubkey,
    creator: &Pubkey,
    token_program: &Pubkey,
    amount: u64,
    min_sol_output: u64,
) -> Instruction {
    let quote_tp = ata::TOKEN_PROGRAM_ID;
    let mut accounts = common_v2_accounts(user, mint, creator, token_program);
    let uva = user_volume_accumulator_pda(user);
    accounts.push(AccountMeta::new_readonly(sharing_config_pda(mint), false)); // 18
    accounts.push(AccountMeta::new(uva, false)); // 19 (no gva on sells)
    accounts.push(AccountMeta::new(
        ata::derive_with_program(&uva, &WSOL_MINT, &quote_tp),
        false,
    )); // 20
    v2_tail(&mut accounts); // 21..=25
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: encode_args(DISCRIM_SELL, amount, min_sol_output),
    }
}

fn encode_args(discriminator: [u8; 8], amount: u64, sol_bound: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&sol_bound.to_le_bytes());
    data
}

// ── bonding curve quote helpers ──────────────────────────────────────

/// PumpFun virtual-reserves at curve genesis. The on-chain account
/// `BondingCurve` exposes these as live fields; if the caller doesn't
/// have RPC handy they can use these initial values to ballpark the
/// pre-first-trade quote (fresh launches only). For ongoing curves
/// always read the live `BondingCurve` account.
///
/// Source: PumpFun Anchor IDL deploy parameters, confirmed against
/// `Initialize` events on mainnet.
pub const INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000; // 30 SOL in lamports
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000; // 1.073 B with 6 dec
pub const INITIAL_REAL_TOKEN_RESERVES: u64 = 793_100_000_000_000; // 793.1 M with 6 dec
pub const MIGRATION_THRESHOLD_LAMPORTS: u64 = 85_000_000_000; // ~85 SOL

/// Compute the expected base-token output for a buy of `sol_in_lamports`
/// against a constant-product curve with the supplied virtual reserves.
///
/// Math: `tokens_out = vtoken - (k / (vsol + sol_in))` where
/// `k = vsol * vtoken` (held constant).
///
/// Returns `None` on integer overflow (would only happen for absurd
/// inputs). Caller MUST pass live reserves from the on-chain
/// BondingCurve account — using the initial constants gives a quote
/// that's only correct on the very first buy.
pub fn buy_quote(
    sol_in_lamports: u64,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
) -> Option<u64> {
    let k = (virtual_sol_reserves as u128).checked_mul(virtual_token_reserves as u128)?;
    let new_sol = (virtual_sol_reserves as u128).checked_add(sol_in_lamports as u128)?;
    let new_tokens = k.checked_div(new_sol)?;
    let tokens_out = (virtual_token_reserves as u128).checked_sub(new_tokens)?;
    if tokens_out > u64::MAX as u128 {
        return None;
    }
    Some(tokens_out as u64)
}

/// Compute the expected SOL output for a sell of `token_in` against
/// the same constant-product curve. See `buy_quote` for the math.
pub fn sell_quote(
    token_in: u64,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
) -> Option<u64> {
    let k = (virtual_sol_reserves as u128).checked_mul(virtual_token_reserves as u128)?;
    let new_tokens = (virtual_token_reserves as u128).checked_add(token_in as u128)?;
    let new_sol = k.checked_div(new_tokens)?;
    let sol_out = (virtual_sol_reserves as u128).checked_sub(new_sol)?;
    if sol_out > u64::MAX as u128 {
        return None;
    }
    Some(sol_out as u64)
}

/// Apply a slippage tolerance (in basis points, 0..10000) to a quoted
/// output. For buys: `max_sol_cost = quote * (1 + bps/10000)`. For
/// sells: `min_sol_output = quote * (1 - bps/10000)`.
pub fn apply_slippage(quote: u64, bps: u16, up: bool) -> u64 {
    let q = quote as u128;
    let adj = q * bps as u128 / 10_000;
    if up {
        (q + adj).min(u64::MAX as u128) as u64
    } else {
        q.saturating_sub(adj) as u64
    }
}

// ── BondingCurve account decoder ─────────────────────────────────────

/// Anchor discriminator for the `BondingCurve` account = first 8 bytes
/// of `sha256("account:BondingCurve")`. Required prefix on every
/// account-data blob; mismatch == not a PumpFun curve account.
pub const BONDING_CURVE_ACCOUNT_DISCRIM: [u8; 8] = [0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60];

/// PumpFun bonding-curve account state, as serialised by the on-chain
/// program. Field order (after the 8-byte discriminator):
/// 5 × u64 LE, `complete: bool` (1 byte), `creator: Pubkey` (32 bytes).
/// Live accounts carry trailing fields beyond `creator`
/// (`is_mayhem_mode`, `is_cashback_coin`, `quote_mint`) which the
/// signer doesn't need; the decoder reads the leading 81 bytes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurveAccount {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    /// True once the curve has graduated to PumpFun-AMM (Pumpswap
    /// pool). Buys against this account would revert; caller must
    /// route via PumpFun-AMM / Jupiter instead.
    pub complete: bool,
    /// Coin creator — seeds the `creator_vault` PDA that receives the
    /// creator's fee share on every swap.
    pub creator: Pubkey,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("data too short ({0} bytes, expected ≥81)")]
    TooShort(usize),
    #[error("discriminator mismatch — not a PumpFun BondingCurve account")]
    BadDiscriminator,
}

/// Decode the on-chain account `data` blob. Caller fetches the account
/// via `getAccountInfo` and passes `account.data` here.
pub fn decode_bonding_curve(data: &[u8]) -> Result<BondingCurveAccount, DecodeError> {
    // 8 disc + 5 × 8 (u64) + 1 (bool) + 32 (creator) = 81 bytes
    // minimum. Live accounts are ~151 (extra fields appended in 2025/26);
    // we only read the leading 81.
    if data.len() < 81 {
        return Err(DecodeError::TooShort(data.len()));
    }
    if data[..8] != BONDING_CURVE_ACCOUNT_DISCRIM {
        return Err(DecodeError::BadDiscriminator);
    }
    let r = |off: usize| u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
    let mut creator = [0u8; 32];
    creator.copy_from_slice(&data[49..81]);
    Ok(BondingCurveAccount {
        virtual_token_reserves: r(8),
        virtual_sol_reserves: r(16),
        real_token_reserves: r(24),
        real_sol_reserves: r(32),
        token_total_supply: r(40),
        complete: data[48] != 0,
        creator: Pubkey::new_from_array(creator),
    })
}

// ── full swap-tx builder (unsigned) ──────────────────────────────────

/// Inputs for `build_buy_tx`. Everything the caller already had to
/// fetch (recent blockhash, live bonding-curve reserves, mint owner
/// program, dynamic priority fee) is passed in; the builder does no I/O.
#[derive(Debug, Clone)]
pub struct BuyTxParams {
    pub user: Pubkey,
    pub mint: Pubkey,
    /// Coin creator from the BondingCurve account (`curve.creator`).
    pub creator: Pubkey,
    /// The mint account's owner — legacy SPL Token or Token-2022.
    pub token_program: Pubkey,
    pub sol_in_lamports: u64,
    pub slippage_bps: u16,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub recent_blockhash: Hash,
    /// Set to ~150_000 for safety. PumpFun buy + ATA-create idempotent
    /// + fee-program CPI + compute-budget ixs typically consume 80-120k.
    pub compute_unit_limit: u32,
    /// micro-lamports per CU. Sized from `/api/trading/stats/priority-fee`
    /// p75 in prod; default ~50_000 is fine for non-contested slots.
    pub compute_unit_price_micro_lamports: u64,
    /// When true, omit the `create_idempotent_ix` for the token ATA.
    /// Set by the caller from an `AtaCache` lookup — saves ~3.5k CU
    /// and ~120 bytes of tx payload per repeat buy on the same
    /// wallet+mint. Leave `false` (default) to preserve the safe
    /// behavior of always pre-pending the create-IX.
    pub skip_token_ata_create: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildTxError {
    #[error("buy_quote returned None (curve overflow / zero reserves)")]
    QuoteUnavailable,
    #[error("v0 message compile failed: {0}")]
    Compile(String),
    #[error("bincode serialise failed: {0}")]
    Bincode(String),
}

/// Build an unsigned v0 VersionedTransaction for a PumpFun buy. Returns
/// the bincode-serialised tx bytes ready to hand to `signer::sign_versioned_tx_bytes`.
/// The instruction ordering is: compute-budget × 2 → idempotent ATA-create
/// → buy. CU-budget MUST come first per Solana's runtime rules.
///
/// PumpFun classic spends NATIVE SOL (the program does the system
/// transfer internally) — no WSOL wrap needed on this venue.
pub fn build_buy_tx(p: &BuyTxParams) -> Result<Vec<u8>, BuildTxError> {
    // 1. Quote against the supplied reserves.
    let expected_out = buy_quote(
        p.sol_in_lamports,
        p.virtual_sol_reserves,
        p.virtual_token_reserves,
    )
    .ok_or(BuildTxError::QuoteUnavailable)?;
    // 2. Apply slippage to derive the `max_sol_cost` upper bound. Note:
    //    for buys, the slippage budget protects the SOL side — the
    //    amount we *want* (`expected_out`) goes in unchanged.
    let max_sol_cost = apply_slippage(p.sol_in_lamports, p.slippage_bps, true);

    let mut ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
    ];
    if !p.skip_token_ata_create {
        // Idempotent — no-ops when the ATA already exists. The caller
        // can suppress this entirely via `skip_token_ata_create` when
        // an `AtaCache` lookup confirms the account already exists,
        // saving ~3.5k CU and ~120 bytes per repeat buy.
        ixs.push(ata::create_idempotent_ix_with_program(
            &p.user,
            &p.user,
            &p.mint,
            &p.token_program,
        ));
    }
    ixs.push(build_buy_ix(
        &p.user,
        &p.mint,
        &p.creator,
        &p.token_program,
        expected_out,
        max_sol_cost,
    ));

    compile_tx(&p.user, ixs, p.recent_blockhash)
}

/// Inputs for `build_sell_tx`. Sells don't need an ATA-create (the
/// user already holds the token), but otherwise the shape mirrors
/// `BuyTxParams`.
#[derive(Debug, Clone)]
pub struct SellTxParams {
    pub user: Pubkey,
    pub mint: Pubkey,
    /// Coin creator from the BondingCurve account (`curve.creator`).
    pub creator: Pubkey,
    /// The mint account's owner — legacy SPL Token or Token-2022.
    pub token_program: Pubkey,
    pub token_in_amount: u64,
    pub slippage_bps: u16,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
}

pub fn build_sell_tx(p: &SellTxParams) -> Result<Vec<u8>, BuildTxError> {
    let expected_sol = sell_quote(
        p.token_in_amount,
        p.virtual_sol_reserves,
        p.virtual_token_reserves,
    )
    .ok_or(BuildTxError::QuoteUnavailable)?;
    let min_sol_output = apply_slippage(expected_sol, p.slippage_bps, false);

    let ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
        build_sell_ix(
            &p.user,
            &p.mint,
            &p.creator,
            &p.token_program,
            p.token_in_amount,
            min_sol_output,
        ),
    ];

    compile_tx(&p.user, ixs, p.recent_blockhash)
}

fn compile_tx(
    payer: &Pubkey,
    ixs: Vec<Instruction>,
    blockhash: Hash,
) -> Result<Vec<u8>, BuildTxError> {
    let msg = v0::Message::try_compile(payer, &ixs, &[], blockhash)
        .map_err(|e| BuildTxError::Compile(e.to_string()))?;
    let unsigned = VersionedTransaction {
        // Placeholder signature — `signer::sign_versioned_tx_bytes`
        // overwrites it.
        signatures: vec![Default::default()],
        message: VersionedMessage::V0(msg),
    };
    bincode::serialize(&unsigned).map_err(|e| BuildTxError::Bincode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    // ─────────────────────────────────────────────────────────────
    // LIVE MAINNET FIXTURES (fetched 2026-06-11 via public RPC)
    //
    // Sell tx sig:
    //   5nqjrxjGvRP5SaX5WfsXBNNq1EripTJZ374wXJznmcGyqjgYe2iyB1T4bQtHo4uZR7Zzuyzz9yQ38n8DFcWQRrze
    //   mint  AAS4ggQ6KjqjNf8LkB27S2DZDQrGo126KfT4cesQpump (Token-2022!)
    //   user  ECsb8YdBDVysWWoiCx6DfdQ4spG7X2nE3uV2vEwKbPDw
    // Buy tx sig:
    //   HAUnBUBSnxKfgqrTqrLDhQmx7e8a149KNFg62U6e8E7FTyr9voqb2chdZpUQ3JWc6TSZeh9MiyiRzeX2M3x53kk
    //   user  EYrMvPMFM6iNmEab9ujvfQCgXfovbsmGFHTSLHRhiPQb
    // ─────────────────────────────────────────────────────────────

    /// `getAccountInfo` data of the live BondingCurve account
    /// 95haiQj2BNxeCQn8admDAaEpSTQ3rs8HnepQYeoNjmMH (mint AAS4…pump),
    /// fetched 2026-06-11. 151 bytes.
    const LIVE_CURVE_B64: &str = "F7f4N2DYrGAUu3cLugYDALz0b8wIAAAAFCNlvygIAgC8SEzQAQAAAACAxqR+jQMAADX7olS83HGkXRsPl+XvXA4LlmTmuJARksd8QtnnVGT4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

    const LIVE_MINT: Pubkey = pubkey!("AAS4ggQ6KjqjNf8LkB27S2DZDQrGo126KfT4cesQpump");
    const LIVE_USER: Pubkey = pubkey!("ECsb8YdBDVysWWoiCx6DfdQ4spG7X2nE3uV2vEwKbPDw");

    fn live_curve() -> BondingCurveAccount {
        let data = base64::engine::general_purpose::STANDARD
            .decode(LIVE_CURVE_B64)
            .unwrap();
        decode_bonding_curve(&data).expect("live curve decodes")
    }

    // ── encoder discriminators ──

    #[test]
    fn v2_discriminators_match_anchor_sighash() {
        // First 8 bytes of sha256("global:buy_v2") / ("global:sell_v2")
        // — cross-checked against the on-chain IDL `discriminator`
        // fields fetched 2026-06-11.
        use sha2::{Digest, Sha256};
        let buy = Sha256::digest(b"global:buy_v2");
        let sell = Sha256::digest(b"global:sell_v2");
        assert_eq!(DISCRIM_BUY, buy[..8]);
        assert_eq!(DISCRIM_SELL, sell[..8]);
    }

    // ── PDA derivations pinned against live mainnet accounts ──

    #[test]
    fn global_pda_matches_live() {
        // Account 0 in every live buy/sell sampled.
        assert_eq!(
            global_pda(),
            pubkey!("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf")
        );
    }

    #[test]
    fn event_authority_pda_matches_live() {
        assert_eq!(
            event_authority_pda(),
            pubkey!("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")
        );
    }

    #[test]
    fn fee_config_pda_matches_live() {
        // Account 14 (buy) / 12 (sell) in the live txs; owned by the
        // fee program; seeds ["fee_config", PUMP_PROGRAM].
        assert_eq!(
            fee_config_pda(),
            pubkey!("8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt")
        );
    }

    #[test]
    fn global_volume_accumulator_pda_matches_live() {
        // Account 12 in both live buys sampled.
        assert_eq!(
            global_volume_accumulator_pda(),
            pubkey!("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y")
        );
    }

    #[test]
    fn user_volume_accumulator_pda_matches_live() {
        // Buy sig HAUnBUBS… user EYrMvPMF… → uva DVj8iwmq….
        assert_eq!(
            user_volume_accumulator_pda(&pubkey!("EYrMvPMFM6iNmEab9ujvfQCgXfovbsmGFHTSLHRhiPQb")),
            pubkey!("DVj8iwmqNjikfsNH9HHQiU4Mb7AQu22Jcptde499WPW7")
        );
        // Buy sig 3s5T1nuq… user CNxRfVAx… → uva B7Mj7BHx….
        assert_eq!(
            user_volume_accumulator_pda(&pubkey!("CNxRfVAxxqjre1CPnnEaojn9r1RMJUk65PqYvR7j1gRW")),
            pubkey!("B7Mj7BHx2ZzVV582itgEtpa1XqCL6MVpXD249u9FHMMq")
        );
    }

    #[test]
    fn creator_vault_pda_matches_live() {
        // The live curve account carries the creator; its vault is
        // account 8 of the live sell (5tKv25Hj…).
        let curve = live_curve();
        assert_eq!(
            creator_vault_pda(&curve.creator),
            pubkey!("5tKv25Hj7hoekg7XRzN4bqUwwLNmEx4exRXFUcmFAWKj")
        );
    }

    // ── full layout pinning: sell_v2 (26 accounts, IDL order) ──

    #[test]
    fn build_sell_ix_matches_v2_idl_layout() {
        // Pin the EXACT account list (order + derivations +
        // writability) against the on-chain IDL's sell_v2 declaration.
        // PDA values that also appear in live legacy txs (bonding
        // curve ATAs, creator_vault) are pinned to those observed
        // addresses — they're shared between legacy and v2.
        let curve = live_curve();
        let ix = build_sell_ix(
            &LIVE_USER,
            &LIVE_MINT,
            &curve.creator,
            &ata::TOKEN_2022_PROGRAM_ID,
            1,
            1,
        );
        assert_eq!(ix.accounts.len(), 26);
        let a = &ix.accounts;
        assert_eq!(a[0].pubkey, global_pda());
        assert_eq!(a[1].pubkey, LIVE_MINT);
        assert_eq!(a[2].pubkey, WSOL_MINT);
        assert_eq!(a[3].pubkey, ata::TOKEN_2022_PROGRAM_ID);
        assert_eq!(a[4].pubkey, ata::TOKEN_PROGRAM_ID);
        assert_eq!(a[5].pubkey, ata::ASSOCIATED_TOKEN_PROGRAM_ID);
        assert_eq!(a[6].pubkey, FEE_RECIPIENT);
        assert!(a[6].is_writable);
        assert_eq!(
            a[7].pubkey,
            ata::derive_with_program(&FEE_RECIPIENT, &WSOL_MINT, &ata::TOKEN_PROGRAM_ID)
        );
        assert_eq!(a[8].pubkey, BUYBACK_FEE_RECIPIENT);
        assert!(a[8].is_writable);
        // The buyback recipient's WSOL ATA — pinned against the LIVE
        // PumpSwap sell tx (sig 3bmEMeqcfKQ4…, account 23) which uses
        // the identical derivation.
        assert_eq!(
            a[9].pubkey,
            pubkey!("HjQjngTDqoHE6aaGhUqfz9aQ7WZcBRjy5xB8PScLSr8i")
        );
        assert_eq!(a[10].pubkey, bonding_curve_pda(&LIVE_MINT));
        assert!(a[10].is_writable);
        // associated_base_bonding_curve under the MINT's token program
        // (Token-2022 here) — matches live legacy sell account 4.
        assert_eq!(
            a[11].pubkey,
            pubkey!("EaooHnA6WDQBKiSQKyJVC3vM8gmx9ZNs3k2BBSxSak7N")
        );
        assert_eq!(
            a[12].pubkey,
            ata::derive_with_program(
                &bonding_curve_pda(&LIVE_MINT),
                &WSOL_MINT,
                &ata::TOKEN_PROGRAM_ID
            )
        );
        assert_eq!(a[13].pubkey, LIVE_USER);
        assert!(a[13].is_signer && a[13].is_writable);
        // associated_base_user — matches live legacy sell account 5.
        assert_eq!(
            a[14].pubkey,
            pubkey!("BLg8xrjNjVTAnCxLHrrKWgWAm2StZ9SmyYrQ74R9sjkR")
        );
        assert_eq!(
            a[15].pubkey,
            ata::derive_with_program(&LIVE_USER, &WSOL_MINT, &ata::TOKEN_PROGRAM_ID)
        );
        // creator_vault — matches live legacy sell account 8.
        assert_eq!(
            a[16].pubkey,
            pubkey!("5tKv25Hj7hoekg7XRzN4bqUwwLNmEx4exRXFUcmFAWKj")
        );
        assert!(a[16].is_writable);
        assert_eq!(
            a[17].pubkey,
            ata::derive_with_program(
                &creator_vault_pda(&curve.creator),
                &WSOL_MINT,
                &ata::TOKEN_PROGRAM_ID
            )
        );
        assert_eq!(a[18].pubkey, sharing_config_pda(&LIVE_MINT));
        assert!(!a[18].is_writable);
        assert_eq!(a[19].pubkey, user_volume_accumulator_pda(&LIVE_USER));
        assert!(a[19].is_writable);
        assert_eq!(
            a[20].pubkey,
            ata::derive_with_program(
                &user_volume_accumulator_pda(&LIVE_USER),
                &WSOL_MINT,
                &ata::TOKEN_PROGRAM_ID
            )
        );
        assert_eq!(a[21].pubkey, fee_config_pda());
        assert_eq!(a[22].pubkey, FEE_PROGRAM_ID);
        assert_eq!(a[23].pubkey, solana_sdk::system_program::ID);
        assert_eq!(a[24].pubkey, event_authority_pda());
        assert_eq!(a[25].pubkey, PROGRAM_ID);
    }

    // ── full layout pinning: buy_v2 (27 accounts, IDL order) ──

    #[test]
    fn build_buy_ix_matches_v2_idl_layout() {
        let curve = live_curve();
        let ix = build_buy_ix(
            &LIVE_USER,
            &LIVE_MINT,
            &curve.creator,
            &ata::TOKEN_2022_PROGRAM_ID,
            1_000_000,
            50_000_000,
        );
        assert_eq!(ix.accounts.len(), 27);
        // Head 0..=17 identical to sell_v2.
        let sell = build_sell_ix(
            &LIVE_USER,
            &LIVE_MINT,
            &curve.creator,
            &ata::TOKEN_2022_PROGRAM_ID,
            1,
            1,
        );
        for i in 0..=18 {
            assert_eq!(
                ix.accounts[i].pubkey, sell.accounts[i].pubkey,
                "head {i} mismatch"
            );
            assert_eq!(
                ix.accounts[i].is_writable, sell.accounts[i].is_writable,
                "head {i} writability"
            );
        }
        // Buys insert the GLOBAL volume accumulator at 19.
        assert_eq!(ix.accounts[19].pubkey, global_volume_accumulator_pda());
        assert!(!ix.accounts[19].is_writable);
        assert_eq!(
            ix.accounts[20].pubkey,
            user_volume_accumulator_pda(&LIVE_USER)
        );
        assert!(ix.accounts[20].is_writable);
        assert_eq!(
            ix.accounts[21].pubkey,
            ata::derive_with_program(
                &user_volume_accumulator_pda(&LIVE_USER),
                &WSOL_MINT,
                &ata::TOKEN_PROGRAM_ID
            )
        );
        assert_eq!(ix.accounts[22].pubkey, fee_config_pda());
        assert_eq!(ix.accounts[23].pubkey, FEE_PROGRAM_ID);
        assert_eq!(ix.accounts[24].pubkey, solana_sdk::system_program::ID);
        assert_eq!(ix.accounts[25].pubkey, event_authority_pda());
        assert_eq!(ix.accounts[26].pubkey, PROGRAM_ID);
    }

    #[test]
    fn build_sell_ix_uses_sell_discriminator() {
        let curve = live_curve();
        let ix = build_sell_ix(
            &LIVE_USER,
            &LIVE_MINT,
            &curve.creator,
            &ata::TOKEN_2022_PROGRAM_ID,
            5_000,
            12_345,
        );
        assert_eq!(&ix.data[..8], &DISCRIM_SELL);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            5_000
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[16..24].try_into().unwrap()),
            12_345
        );
        // 24 bytes — v2 args are exactly (amount, sol_bound), no
        // track_volume byte.
        assert_eq!(ix.data.len(), 24);
    }

    #[test]
    fn pda_derivation_is_stable() {
        let mint = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(bonding_curve_pda(&mint), bonding_curve_pda(&mint));
        assert_eq!(global_pda(), global_pda());
        assert_eq!(event_authority_pda(), event_authority_pda());
    }

    // ── bonding-curve math ──

    #[test]
    fn initial_curve_buy_quote_sanity() {
        let out = buy_quote(
            1_000_000_000, // 1 SOL
            INITIAL_VIRTUAL_SOL_RESERVES,
            INITIAL_VIRTUAL_TOKEN_RESERVES,
        )
        .expect("quote should succeed");
        assert!(
            (34_000_000_000_000..=35_000_000_000_000).contains(&out),
            "got {out}"
        );
    }

    #[test]
    fn buy_then_sell_roundtrip_is_close_to_lossless() {
        let vsol_0 = INITIAL_VIRTUAL_SOL_RESERVES;
        let vtok_0 = INITIAL_VIRTUAL_TOKEN_RESERVES;
        let sol_in = 100_000_000; // 0.1 SOL
        let tokens = buy_quote(sol_in, vsol_0, vtok_0).unwrap();

        let vsol_1 = vsol_0 + sol_in;
        let vtok_1 = vtok_0 - tokens;
        let sol_back = sell_quote(tokens, vsol_1, vtok_1).unwrap();
        let drift = sol_back.abs_diff(sol_in);
        assert!(
            drift <= 1,
            "round-trip drift = {drift} lamports (sol_in={sol_in}, sol_back={sol_back})"
        );
    }

    #[test]
    fn buy_quote_overflow_guard_returns_none_on_zero_reserves() {
        assert_eq!(
            buy_quote(
                0,
                INITIAL_VIRTUAL_SOL_RESERVES,
                INITIAL_VIRTUAL_TOKEN_RESERVES
            ),
            Some(0)
        );
        assert_eq!(buy_quote(0, 0, INITIAL_VIRTUAL_TOKEN_RESERVES), None);
    }

    #[test]
    fn slippage_up_inflates_for_buys() {
        assert_eq!(apply_slippage(1_000_000, 100, true), 1_010_000);
    }

    #[test]
    fn slippage_down_deflates_for_sells() {
        assert_eq!(apply_slippage(1_000_000, 100, false), 990_000);
    }

    #[test]
    fn slippage_zero_is_noop() {
        assert_eq!(apply_slippage(123_456_789, 0, true), 123_456_789);
        assert_eq!(apply_slippage(123_456_789, 0, false), 123_456_789);
    }

    // ── BondingCurve account decoder ──

    #[test]
    fn decode_live_bonding_curve_fixture() {
        // The live 151-byte account decodes; `complete=false` (curve
        // still trading at fetch time) and the creator derives the
        // creator_vault observed in the live sell tx.
        let bc = live_curve();
        assert!(!bc.complete);
        assert!(bc.virtual_token_reserves > 0);
        assert!(bc.virtual_sol_reserves > 0);
        assert_ne!(bc.creator, Pubkey::default());
    }

    fn fake_curve_data(complete: bool) -> Vec<u8> {
        let mut d = Vec::with_capacity(81);
        d.extend_from_slice(&BONDING_CURVE_ACCOUNT_DISCRIM);
        d.extend_from_slice(&1_000_000_000_000_000u64.to_le_bytes()); // vtok
        d.extend_from_slice(&30_000_000_000u64.to_le_bytes()); // vsol
        d.extend_from_slice(&800_000_000_000_000u64.to_le_bytes()); // rtok
        d.extend_from_slice(&5_000_000_000u64.to_le_bytes()); // rsol
        d.extend_from_slice(&1_000_000_000_000_000u64.to_le_bytes()); // supply
        d.push(if complete { 1 } else { 0 });
        d.extend_from_slice(&[0x42; 32]); // creator
        d
    }

    #[test]
    fn decode_bonding_curve_happy_path() {
        let data = fake_curve_data(false);
        let bc = decode_bonding_curve(&data).expect("decode");
        assert_eq!(bc.virtual_token_reserves, 1_000_000_000_000_000);
        assert_eq!(bc.virtual_sol_reserves, 30_000_000_000);
        assert_eq!(bc.real_token_reserves, 800_000_000_000_000);
        assert_eq!(bc.real_sol_reserves, 5_000_000_000);
        assert!(!bc.complete);
        assert_eq!(bc.creator, Pubkey::new_from_array([0x42; 32]));
    }

    #[test]
    fn decode_bonding_curve_picks_up_graduated_flag() {
        let data = fake_curve_data(true);
        let bc = decode_bonding_curve(&data).expect("decode");
        assert!(bc.complete);
    }

    #[test]
    fn decode_bonding_curve_rejects_bad_discriminator() {
        let mut data = fake_curve_data(false);
        data[0] = 0xff;
        assert!(matches!(
            decode_bonding_curve(&data),
            Err(DecodeError::BadDiscriminator)
        ));
    }

    #[test]
    fn decode_bonding_curve_rejects_short_blob() {
        // Pre-2025 49-byte curves (no creator) are also rejected — the
        // live program always serialises the creator field.
        let short = vec![0u8; 49];
        assert!(matches!(
            decode_bonding_curve(&short),
            Err(DecodeError::TooShort(49))
        ));
    }

    // ── full swap-tx build ──

    fn buy_params() -> BuyTxParams {
        let curve = live_curve();
        BuyTxParams {
            user: LIVE_USER,
            mint: LIVE_MINT,
            creator: curve.creator,
            token_program: ata::TOKEN_2022_PROGRAM_ID,
            sol_in_lamports: 100_000_000, // 0.1 SOL
            slippage_bps: 100,            // 1%
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            compute_unit_limit: 150_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_token_ata_create: false,
        }
    }

    #[test]
    fn build_buy_tx_produces_valid_v0_versioned_tx_bytes() {
        let bytes = build_buy_tx(&buy_params()).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 4 instructions in the v0 message: cu-limit, cu-price, ATA, buy.
        match &tx.message {
            VersionedMessage::V0(msg) => {
                assert_eq!(msg.instructions.len(), 4);
            }
            _ => panic!("expected v0 message"),
        }
        assert_eq!(tx.signatures.len(), 1);
    }

    #[test]
    fn build_buy_tx_skips_ata_when_flag_set() {
        let mut p = buy_params();
        p.skip_token_ata_create = true;
        let bytes = build_buy_tx(&p).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            VersionedMessage::V0(msg) => assert_eq!(msg.instructions.len(), 3),
            _ => panic!("expected v0 message"),
        }
    }

    #[test]
    fn build_sell_tx_skips_ata_create() {
        let curve = live_curve();
        let p = SellTxParams {
            user: LIVE_USER,
            mint: LIVE_MINT,
            creator: curve.creator,
            token_program: ata::TOKEN_2022_PROGRAM_ID,
            token_in_amount: 1_000_000_000,
            slippage_bps: 100,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            recent_blockhash: solana_sdk::hash::Hash::default(),
            compute_unit_limit: 120_000,
            compute_unit_price_micro_lamports: 50_000,
        };
        let bytes = build_sell_tx(&p).expect("build_sell_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 3 instructions: cu-limit, cu-price, sell. No ATA-create.
        match &tx.message {
            VersionedMessage::V0(msg) => {
                assert_eq!(msg.instructions.len(), 3);
            }
            _ => panic!("expected v0 message"),
        }
    }

    #[test]
    fn args_decode_roundtrip() {
        let curve = live_curve();
        let ix = build_buy_ix(
            &LIVE_USER,
            &LIVE_MINT,
            &curve.creator,
            &ata::TOKEN_PROGRAM_ID,
            42_424_242,
            7_777_777,
        );
        assert_eq!(&ix.data[..8], &DISCRIM_BUY);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            42_424_242
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[16..24].try_into().unwrap()),
            7_777_777
        );
    }
}
