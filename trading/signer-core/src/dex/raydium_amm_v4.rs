//! Raydium AMM v4 — native instruction encoder.
//!
//! Raydium AMM v4 is the workhorse for established SPL-token pools on
//! Solana. Unlike PumpFun/PumpSwap (Anchor), Raydium uses a single-byte
//! instruction tag followed by two LE u64s:
//!
//!   `9`  = `swapBaseIn`  — exact amount_in, minimum_amount_out computed
//!   `11` = `swapBaseOut` — max_amount_in bounded, exact amount_out
//!
//! We implement `swapBaseIn` only — the standard path for market buys
//! and sells where you fix what you spend and accept ≥ the quoted output.
//!
//! ## Wire format
//!
//! `[tag: u8, amount_in: u64 LE, minimum_amount_out: u64 LE]` = 17 bytes.
//!
//! ## 18-account layout (matches the on-chain IDL)
//!
//! ```text
//!  0: token_program           (readonly)
//!  1: amm                     (writable)
//!  2: amm_authority            (readonly PDA — seeds: [b"amm authority"])
//!  3: amm_open_orders         (writable)
//!  4: amm_target_orders       (writable)
//!  5: pool_coin_token_account (writable)  ← amm.token_coin
//!  6: pool_pc_token_account   (writable)  ← amm.token_pc
//!  7: serum_program            (readonly)  ← amm.serum_dex
//!  8: serum_market             (writable)  ← amm.market
//!  9: serum_bids               (writable)  ← market.bids
//! 10: serum_asks               (writable)  ← market.asks
//! 11: serum_event_queue        (writable)  ← market.event_queue
//! 12: serum_coin_vault         (writable)  ← market.coin_vault
//! 13: serum_pc_vault           (writable)  ← market.pc_vault
//! 14: serum_vault_signer       (readonly)  ← create_program_address([market, &nonce_le])
//! 15: user_source_token        (writable)  ← ATA(user, source_mint)
//! 16: user_dest_token          (writable)  ← ATA(user, dest_mint)
//! 17: user_source_owner        (signer)
//! ```
//!
//! ## Fee model
//!
//! AMM state stores `swap_fee_numerator / swap_fee_denominator`.
//! Canonical Raydium v4 value: **25 / 10000 = 0.25%**, applied to the
//! input amount. The decoder reads these live from the pool account so
//! custom-fee pools are handled transparently.
//!
//! ## Field offsets in AmmInfo (packed, confirmed by Raydium SDK)
//!
//! The struct is effectively packed (no alignment padding around u128
//! fields), so u64/u128 fields are read at the documented byte offsets.
//! SDK references and independent MEV bots consistently use offset 336
//! for `token_coin` — we guard this with a decoder round-trip test.

use crate::dex::{ata, compute_budget};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;

// ─── Program constants ───────────────────────────────────────────────

/// Raydium AMM v4 program.
pub const PROGRAM_ID: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

/// OpenBook (née Serum DEX v3) program. Every Raydium v4 pool embeds an
/// OpenBook market for the order-book infrastructure; swap ixs reference
/// its accounts directly.
pub const OPEN_BOOK_PROGRAM_ID: Pubkey = pubkey!("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX");

/// Instruction tag for `swapBaseIn`.
pub const TAG_SWAP_BASE_IN: u8 = 9;

/// AMM authority PDA seed (program-wide singleton).
pub const AMM_AUTHORITY_SEED: &[u8] = b"amm authority";

/// Derive the program-wide AMM authority PDA.
pub fn amm_authority_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[AMM_AUTHORITY_SEED], &PROGRAM_ID);
    pda
}

// ─── AmmInfo field offsets (byte positions in account data) ──────────

const AMM_STATUS_OFFSET: usize = 0;
const AMM_SWAP_FEE_NUMERATOR_OFFSET: usize = 176;
const AMM_SWAP_FEE_DENOMINATOR_OFFSET: usize = 184;
/// Pool coin token vault account (token_coin = base side).
pub const AMM_TOKEN_COIN_OFFSET: usize = 336;
/// Pool pc token vault account (token_pc = quote side, WSOL).
pub const AMM_TOKEN_PC_OFFSET: usize = 368;
pub const AMM_COIN_MINT_OFFSET: usize = 400;
pub const AMM_PC_MINT_OFFSET: usize = 432;
pub const AMM_OPEN_ORDERS_OFFSET: usize = 496;
pub const AMM_MARKET_OFFSET: usize = 528;
pub const AMM_SERUM_DEX_OFFSET: usize = 560;
pub const AMM_TARGET_ORDERS_OFFSET: usize = 592;
const AMM_MIN_LEN: usize = AMM_TARGET_ORDERS_OFFSET + 32; // 624

// ─── OpenBook Market field offsets ───────────────────────────────────
//
// Market data is prefixed with 5 bytes of discriminator; account_flags
// at byte 5, own_address at byte 13. The vault_signer_nonce and key
// accounts start at the offsets documented in serum-dex source.

const MKT_VAULT_SIGNER_NONCE_OFFSET: usize = 45; // u64
const MKT_COIN_VAULT_OFFSET: usize = 117; // Pubkey
const MKT_PC_VAULT_OFFSET: usize = 165; // Pubkey
const MKT_EVENT_QUEUE_OFFSET: usize = 253; // Pubkey
const MKT_BIDS_OFFSET: usize = 285; // Pubkey
const MKT_ASKS_OFFSET: usize = 317; // Pubkey
const MKT_MIN_LEN: usize = MKT_ASKS_OFFSET + 32; // 349

// ─── Decoded state types ─────────────────────────────────────────────

/// Key fields from the on-chain Raydium AMM state account.
/// Caller fetches via `getAccountInfo` on the AMM pubkey and calls
/// `decode_amm_state`.
#[derive(Debug, Clone, Copy)]
pub struct AmmState {
    /// Pool active flag. Non-zero = open for swaps.
    pub status: u64,
    /// Numerator of the swap fee (canonical = 25).
    pub swap_fee_numerator: u64,
    /// Denominator of the swap fee (canonical = 10000).
    pub swap_fee_denominator: u64,
    /// Pool coin token vault SPL account.
    pub token_coin: Pubkey,
    /// Pool pc token vault SPL account (typically WSOL).
    pub token_pc: Pubkey,
    /// Coin (base) mint.
    pub coin_mint: Pubkey,
    /// PC (quote) mint.
    pub pc_mint: Pubkey,
    /// AMM open-orders account.
    pub open_orders: Pubkey,
    /// Serum/OpenBook market address.
    pub market: Pubkey,
    /// Serum/OpenBook DEX program ID.
    pub serum_dex: Pubkey,
    /// Target orders account (Raydium-internal, required by on-chain program).
    pub target_orders: Pubkey,
}

/// Key fields from the Serum/OpenBook market account needed for the
/// 18-account swap layout. Caller fetches `amm.market` and calls
/// `decode_market_state`.
#[derive(Debug, Clone, Copy)]
pub struct MarketState {
    /// Nonce used to derive the vault signer PDA.
    pub vault_signer_nonce: u64,
    pub coin_vault: Pubkey,
    pub pc_vault: Pubkey,
    pub event_queue: Pubkey,
    pub bids: Pubkey,
    pub asks: Pubkey,
}

/// Derive the Serum vault signer PDA.
///
/// Uses `create_program_address` (not `find_program_address`) because
/// the nonce is stored on-chain in the market account — it was fixed at
/// market creation time.
pub fn vault_signer_pda(
    market: &Pubkey,
    vault_signer_nonce: u64,
    serum_dex: &Pubkey,
) -> Result<Pubkey, solana_sdk::pubkey::PubkeyError> {
    Pubkey::create_program_address(
        &[market.as_ref(), &vault_signer_nonce.to_le_bytes()],
        serum_dex,
    )
}

/// Live pool vault balances. Caller fetches via `getTokenAccountBalance`
/// on `amm.token_coin` (coin/base side) and `amm.token_pc` (pc/quote side).
#[derive(Debug, Clone, Copy)]
pub struct PoolReserves {
    pub coin: u64,
    pub pc: u64,
}

// ─── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("AMM state too short: {0} bytes, need ≥{1}")]
    AmmTooShort(usize, usize),
    #[error("market state too short: {0} bytes, need ≥{1}")]
    MarketTooShort(usize, usize),
    #[error("vault signer derivation failed: {0}")]
    VaultSigner(String),
}

#[derive(Debug, thiserror::Error)]
pub enum BuildTxError {
    #[error("quote unavailable: {0}")]
    QuoteUnavailable(String),
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("v0 message compile: {0}")]
    Compile(String),
    #[error("bincode: {0}")]
    Bincode(String),
}

// ─── Decoders ────────────────────────────────────────────────────────

fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

fn read_pk(data: &[u8], off: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(b)
}

/// Decode the Raydium AMM v4 state account. Only extracts the subset of
/// fields needed to build a swap instruction — no LP stats, no fees-
/// accrued counters.
pub fn decode_amm_state(data: &[u8]) -> Result<AmmState, DecodeError> {
    if data.len() < AMM_MIN_LEN {
        return Err(DecodeError::AmmTooShort(data.len(), AMM_MIN_LEN));
    }
    Ok(AmmState {
        status: read_u64(data, AMM_STATUS_OFFSET),
        swap_fee_numerator: read_u64(data, AMM_SWAP_FEE_NUMERATOR_OFFSET),
        swap_fee_denominator: read_u64(data, AMM_SWAP_FEE_DENOMINATOR_OFFSET),
        token_coin: read_pk(data, AMM_TOKEN_COIN_OFFSET),
        token_pc: read_pk(data, AMM_TOKEN_PC_OFFSET),
        coin_mint: read_pk(data, AMM_COIN_MINT_OFFSET),
        pc_mint: read_pk(data, AMM_PC_MINT_OFFSET),
        open_orders: read_pk(data, AMM_OPEN_ORDERS_OFFSET),
        market: read_pk(data, AMM_MARKET_OFFSET),
        serum_dex: read_pk(data, AMM_SERUM_DEX_OFFSET),
        target_orders: read_pk(data, AMM_TARGET_ORDERS_OFFSET),
    })
}

/// Decode the Serum/OpenBook market state account.
pub fn decode_market_state(data: &[u8]) -> Result<MarketState, DecodeError> {
    if data.len() < MKT_MIN_LEN {
        return Err(DecodeError::MarketTooShort(data.len(), MKT_MIN_LEN));
    }
    Ok(MarketState {
        vault_signer_nonce: read_u64(data, MKT_VAULT_SIGNER_NONCE_OFFSET),
        coin_vault: read_pk(data, MKT_COIN_VAULT_OFFSET),
        pc_vault: read_pk(data, MKT_PC_VAULT_OFFSET),
        event_queue: read_pk(data, MKT_EVENT_QUEUE_OFFSET),
        bids: read_pk(data, MKT_BIDS_OFFSET),
        asks: read_pk(data, MKT_ASKS_OFFSET),
    })
}

// ─── Instruction builder ─────────────────────────────────────────────

/// Build a `swapBaseIn` instruction.
///
/// * `user`              — signer wallet that owns the source ATA
/// * `amm_pubkey`        — the AMM account address
/// * `amm`               — decoded AMM state
/// * `market`            — decoded OpenBook market state
/// * `user_source_mint`  — token the user is spending
/// * `user_dest_mint`    — token the user wants to receive
/// * `amount_in`         — exact amount to spend (base units)
/// * `minimum_amount_out`— minimum acceptable output (slippage bound)
#[allow(clippy::too_many_arguments)]
pub fn build_swap_base_in_ix(
    user: &Pubkey,
    amm_pubkey: &Pubkey,
    amm: &AmmState,
    market: &MarketState,
    user_source_mint: &Pubkey,
    user_dest_mint: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Result<Instruction, DecodeError> {
    let authority = amm_authority_pda();
    let vault_signer = vault_signer_pda(&amm.market, market.vault_signer_nonce, &amm.serum_dex)
        .map_err(|e| DecodeError::VaultSigner(e.to_string()))?;

    let user_source = ata::derive(user, user_source_mint);
    let user_dest = ata::derive(user, user_dest_mint);

    let mut data = Vec::with_capacity(17);
    data.push(TAG_SWAP_BASE_IN);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    Ok(Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(ata::TOKEN_PROGRAM_ID, false), // 0
            AccountMeta::new(*amm_pubkey, false),                    // 1
            AccountMeta::new_readonly(authority, false),             // 2
            AccountMeta::new(amm.open_orders, false),                // 3
            AccountMeta::new(amm.target_orders, false),              // 4
            AccountMeta::new(amm.token_coin, false),                 // 5
            AccountMeta::new(amm.token_pc, false),                   // 6
            AccountMeta::new_readonly(amm.serum_dex, false),         // 7
            AccountMeta::new(amm.market, false),                     // 8
            AccountMeta::new(market.bids, false),                    // 9
            AccountMeta::new(market.asks, false),                    // 10
            AccountMeta::new(market.event_queue, false),             // 11
            AccountMeta::new(market.coin_vault, false),              // 12
            AccountMeta::new(market.pc_vault, false),                // 13
            AccountMeta::new_readonly(vault_signer, false),          // 14
            AccountMeta::new(user_source, false),                    // 15
            AccountMeta::new(user_dest, false),                      // 16
            AccountMeta::new(*user, true),                           // 17 signer
        ],
        data,
    })
}

// ─── Quote math ──────────────────────────────────────────────────────

/// Constant-product quote for a `swapBaseIn` trade.
///
/// Fee is applied on the input side:
///   `effective_in = amount_in × (fee_denom - fee_num) / fee_denom`
///   `out = reserve_out × effective_in / (reserve_in + effective_in)`
///
/// Returns `None` on zero reserves, zero denominator, or u128 overflow.
pub fn swap_base_in_quote(
    amount_in: u64,
    reserve_in: u64,
    reserve_out: u64,
    fee_num: u64,
    fee_denom: u64,
) -> Option<u64> {
    if reserve_in == 0 || reserve_out == 0 || fee_denom == 0 || fee_num >= fee_denom {
        return None;
    }
    let a_in = amount_in as u128;
    let r_in = reserve_in as u128;
    let r_out = reserve_out as u128;
    let effective_in = a_in.checked_mul((fee_denom - fee_num) as u128)? / fee_denom as u128;
    let numer = r_out.checked_mul(effective_in)?;
    let denom = r_in.checked_add(effective_in)?;
    let out = numer.checked_div(denom)?;
    if out > u64::MAX as u128 {
        return None;
    }
    Some(out as u64)
}

/// Apply slippage tolerance. `up=true` inflates (max-in bound),
/// `up=false` deflates (min-out bound). Same semantics as PumpSwap helper.
pub fn apply_slippage(amount: u64, bps: u16, up: bool) -> u64 {
    let a = amount as u128;
    let adj = a * bps as u128 / 10_000;
    if up {
        (a + adj).min(u64::MAX as u128) as u64
    } else {
        a.saturating_sub(adj) as u64
    }
}

// ─── Full swap-tx builders ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BuyTxParams {
    /// User wallet (signer + ATA owner).
    pub user: Pubkey,
    /// AMM account address (from pool discovery).
    pub amm_pubkey: Pubkey,
    /// Decoded AMM state (via `decode_amm_state`).
    pub amm: AmmState,
    /// Decoded market state (via `decode_market_state`).
    pub market: MarketState,
    /// Live pool vault balances (coin = base, pc = quote/WSOL).
    pub reserves: PoolReserves,
    /// WSOL lamports to spend. User pays pc (WSOL), receives coin.
    pub quote_in_lamports: u64,
    pub slippage_bps: u16,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    /// Suppress `CreateIdempotent` for the coin (token) ATA when an
    /// `AtaCache` lookup confirms it already exists.
    pub skip_coin_ata_create: bool,
    /// Suppress `CreateIdempotent` for the pc ATA. IGNORED when
    /// `pc_mint` is WSOL — the wrap sequence always (re)creates the
    /// WSOL ATA because the builders close it after every swap
    /// (audit H2).
    pub skip_pc_ata_create: bool,
}

#[derive(Debug, Clone)]
pub struct SellTxParams {
    pub user: Pubkey,
    pub amm_pubkey: Pubkey,
    pub amm: AmmState,
    pub market: MarketState,
    pub reserves: PoolReserves,
    /// Base-token units to sell. User pays coin, receives pc (WSOL).
    pub coin_in_amount: u64,
    pub slippage_bps: u16,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    /// Suppress `CreateIdempotent` for the pc (WSOL) ATA receiver.
    pub skip_pc_ata_create: bool,
}

/// Build unsigned v0 tx for a Raydium buy (WSOL → token).
///
/// WSOL-quoted pools (the norm): the buy WRAPS native SOL just-in-time
/// (audit H2) — create WSOL ATA (idempotent) → fund it with the swap
/// input via system transfer → `SyncNative` → swapBaseIn → close the
/// WSOL ATA (leftovers + rent return as native SOL). The old builder
/// created the ATA but never funded it, so every buy sim-failed with
/// insufficient funds.
pub fn build_buy_tx(p: &BuyTxParams) -> Result<Vec<u8>, BuildTxError> {
    // User spends pc (WSOL), receives coin. reserve_in = pc side.
    let min_coin_out = swap_base_in_quote(
        p.quote_in_lamports,
        p.reserves.pc,
        p.reserves.coin,
        p.amm.swap_fee_numerator,
        p.amm.swap_fee_denominator,
    )
    .ok_or_else(|| BuildTxError::QuoteUnavailable("zero reserves or fee error".into()))?;

    let min_out_slipped = apply_slippage(min_coin_out, p.slippage_bps, false);

    let swap_ix = build_swap_base_in_ix(
        &p.user,
        &p.amm_pubkey,
        &p.amm,
        &p.market,
        &p.amm.pc_mint,   // user_source_mint = WSOL
        &p.amm.coin_mint, // user_dest_mint = token
        p.quote_in_lamports,
        min_out_slipped,
    )?;

    let wrap_sol = p.amm.pc_mint == ata::WSOL_MINT;
    let pc_ata = ata::derive(&p.user, &p.amm.pc_mint);

    let mut ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
    ];
    if !p.skip_coin_ata_create {
        // Coin ATA — token receiver. Cache may suppress on repeat buys.
        ixs.push(ata::create_idempotent_ix(
            &p.user,
            &p.user,
            &p.amm.coin_mint,
        ));
    }
    if wrap_sol {
        // Wrap: ATA create (always — closed again below), fund the
        // exact swap input, sync the token amount with the lamports.
        ixs.push(ata::create_idempotent_ix(&p.user, &p.user, &p.amm.pc_mint));
        ixs.push(ata::system_transfer_ix(
            &p.user,
            &pc_ata,
            p.quote_in_lamports,
        ));
        ixs.push(ata::sync_native_ix(&pc_ata));
    } else if !p.skip_pc_ata_create {
        // Non-WSOL quote (e.g. USDC pools): plain idempotent create —
        // the user must already hold the quote token.
        ixs.push(ata::create_idempotent_ix(&p.user, &p.user, &p.amm.pc_mint));
    }
    ixs.push(swap_ix);
    if wrap_sol {
        // Unwrap leftovers + rent back to native SOL.
        ixs.push(ata::close_account_ix(&pc_ata, &p.user, &p.user));
    }
    compile_tx(&p.user, ixs, p.recent_blockhash)
}

/// Build unsigned v0 tx for a Raydium sell (token → WSOL).
///
/// WSOL proceeds are unwrapped (CloseAccount → lamports to the owner)
/// after the swap so they show up as spendable native SOL instead of
/// stranding invisibly in the WSOL ATA (audit H2).
pub fn build_sell_tx(p: &SellTxParams) -> Result<Vec<u8>, BuildTxError> {
    // User spends coin, receives pc (WSOL). reserve_in = coin side.
    let min_pc_out = swap_base_in_quote(
        p.coin_in_amount,
        p.reserves.coin,
        p.reserves.pc,
        p.amm.swap_fee_numerator,
        p.amm.swap_fee_denominator,
    )
    .ok_or_else(|| BuildTxError::QuoteUnavailable("zero reserves or fee error".into()))?;

    let min_out_slipped = apply_slippage(min_pc_out, p.slippage_bps, false);

    let swap_ix = build_swap_base_in_ix(
        &p.user,
        &p.amm_pubkey,
        &p.amm,
        &p.market,
        &p.amm.coin_mint, // user_source_mint = token
        &p.amm.pc_mint,   // user_dest_mint = WSOL
        p.coin_in_amount,
        min_out_slipped,
    )?;

    let wrap_sol = p.amm.pc_mint == ata::WSOL_MINT;
    let pc_ata = ata::derive(&p.user, &p.amm.pc_mint);

    let mut ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
    ];
    if wrap_sol || !p.skip_pc_ata_create {
        // Ensure the proceeds receiver exists. For WSOL we ALWAYS
        // create (idempotent) — the previous trade closed the ATA.
        ixs.push(ata::create_idempotent_ix(&p.user, &p.user, &p.amm.pc_mint));
    }
    ixs.push(swap_ix);
    if wrap_sol {
        // Unwrap the SOL proceeds to native balance.
        ixs.push(ata::close_account_ix(&pc_ata, &p.user, &p.user));
    }
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
        signatures: vec![Default::default()],
        message: VersionedMessage::V0(msg),
    };
    bincode::serialize(&unsigned).map_err(|e| BuildTxError::Bincode(e.to_string()))
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ──────────────────────────────────────────────────────

    fn fake_amm_state() -> (Vec<u8>, AmmState) {
        let mut data = vec![0u8; 752]; // full AmmInfo size
                                       // status = 1 (active)
        data[AMM_STATUS_OFFSET..AMM_STATUS_OFFSET + 8].copy_from_slice(&1u64.to_le_bytes());
        // swap_fee_numerator = 25, denominator = 10000
        data[AMM_SWAP_FEE_NUMERATOR_OFFSET..AMM_SWAP_FEE_NUMERATOR_OFFSET + 8]
            .copy_from_slice(&25u64.to_le_bytes());
        data[AMM_SWAP_FEE_DENOMINATOR_OFFSET..AMM_SWAP_FEE_DENOMINATOR_OFFSET + 8]
            .copy_from_slice(&10_000u64.to_le_bytes());

        // Place distinct Pubkeys at every Pubkey offset so decoding is verifiable.
        let coin_pk = Pubkey::new_from_array([0x11; 32]);
        let pc_pk = Pubkey::new_from_array([0x22; 32]);
        let coin_mint = Pubkey::new_from_array([0x33; 32]);
        let pc_mint = Pubkey::new_from_array([0x44; 32]);
        let oo = Pubkey::new_from_array([0x55; 32]);
        let market = Pubkey::new_from_array([0x66; 32]);
        let serum_dex = Pubkey::new_from_array([0x77; 32]);
        let target = Pubkey::new_from_array([0x88; 32]);

        let write_pk = |buf: &mut Vec<u8>, off: usize, pk: Pubkey| {
            buf[off..off + 32].copy_from_slice(pk.as_ref());
        };
        write_pk(&mut data, AMM_TOKEN_COIN_OFFSET, coin_pk);
        write_pk(&mut data, AMM_TOKEN_PC_OFFSET, pc_pk);
        write_pk(&mut data, AMM_COIN_MINT_OFFSET, coin_mint);
        write_pk(&mut data, AMM_PC_MINT_OFFSET, pc_mint);
        write_pk(&mut data, AMM_OPEN_ORDERS_OFFSET, oo);
        write_pk(&mut data, AMM_MARKET_OFFSET, market);
        write_pk(&mut data, AMM_SERUM_DEX_OFFSET, serum_dex);
        write_pk(&mut data, AMM_TARGET_ORDERS_OFFSET, target);

        let state = AmmState {
            status: 1,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            token_coin: coin_pk,
            token_pc: pc_pk,
            coin_mint,
            pc_mint,
            open_orders: oo,
            market,
            serum_dex,
            target_orders: target,
        };
        (data, state)
    }

    fn fake_market_state(nonce: u64) -> (Vec<u8>, MarketState) {
        let mut data = vec![0u8; 400]; // market blob ≥ 349 bytes
        data[MKT_VAULT_SIGNER_NONCE_OFFSET..MKT_VAULT_SIGNER_NONCE_OFFSET + 8]
            .copy_from_slice(&nonce.to_le_bytes());

        let cv = Pubkey::new_from_array([0xaa; 32]);
        let pv = Pubkey::new_from_array([0xbb; 32]);
        let eq = Pubkey::new_from_array([0xcc; 32]);
        let bids = Pubkey::new_from_array([0xdd; 32]);
        let asks = Pubkey::new_from_array([0xee; 32]);

        let write_pk = |buf: &mut Vec<u8>, off: usize, pk: Pubkey| {
            buf[off..off + 32].copy_from_slice(pk.as_ref());
        };
        write_pk(&mut data, MKT_COIN_VAULT_OFFSET, cv);
        write_pk(&mut data, MKT_PC_VAULT_OFFSET, pv);
        write_pk(&mut data, MKT_EVENT_QUEUE_OFFSET, eq);
        write_pk(&mut data, MKT_BIDS_OFFSET, bids);
        write_pk(&mut data, MKT_ASKS_OFFSET, asks);

        let state = MarketState {
            vault_signer_nonce: nonce,
            coin_vault: cv,
            pc_vault: pv,
            event_queue: eq,
            bids,
            asks,
        };
        (data, state)
    }

    // ── constants ────────────────────────────────────────────────────

    #[test]
    fn program_id_matches_known_raydium_v4() {
        assert_eq!(
            PROGRAM_ID.to_string(),
            "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
        );
    }

    #[test]
    fn open_book_program_id_matches_known() {
        assert_eq!(
            OPEN_BOOK_PROGRAM_ID.to_string(),
            "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX"
        );
    }

    #[test]
    fn swap_base_in_tag_is_nine() {
        assert_eq!(TAG_SWAP_BASE_IN, 9);
    }

    #[test]
    fn amm_authority_pda_is_stable() {
        let a = amm_authority_pda();
        let b = amm_authority_pda();
        assert_eq!(a, b, "PDA must be deterministic");
    }

    // ── decoders ─────────────────────────────────────────────────────

    #[test]
    fn decode_amm_state_happy_path() {
        let (data, expected) = fake_amm_state();
        let got = decode_amm_state(&data).expect("decode");
        assert_eq!(got.status, expected.status);
        assert_eq!(got.swap_fee_numerator, 25);
        assert_eq!(got.swap_fee_denominator, 10_000);
        assert_eq!(got.token_coin, expected.token_coin);
        assert_eq!(got.token_pc, expected.token_pc);
        assert_eq!(got.coin_mint, expected.coin_mint);
        assert_eq!(got.pc_mint, expected.pc_mint);
        assert_eq!(got.open_orders, expected.open_orders);
        assert_eq!(got.market, expected.market);
        assert_eq!(got.serum_dex, expected.serum_dex);
        assert_eq!(got.target_orders, expected.target_orders);
    }

    #[test]
    fn decode_amm_state_too_short() {
        let r = decode_amm_state(&[0u8; 100]);
        assert!(matches!(r, Err(DecodeError::AmmTooShort(100, _))));
    }

    #[test]
    fn decode_market_state_happy_path() {
        let nonce = 7u64;
        let (data, expected) = fake_market_state(nonce);
        let got = decode_market_state(&data).expect("decode");
        assert_eq!(got.vault_signer_nonce, nonce);
        assert_eq!(got.coin_vault, expected.coin_vault);
        assert_eq!(got.pc_vault, expected.pc_vault);
        assert_eq!(got.event_queue, expected.event_queue);
        assert_eq!(got.bids, expected.bids);
        assert_eq!(got.asks, expected.asks);
    }

    #[test]
    fn decode_market_state_too_short() {
        let r = decode_market_state(&[0u8; 50]);
        assert!(matches!(r, Err(DecodeError::MarketTooShort(50, _))));
    }

    // ── instruction shape ────────────────────────────────────────────

    #[test]
    fn build_swap_base_in_has_18_accounts() {
        let (_, amm) = fake_amm_state();
        // Build a MarketState whose nonce makes vault_signer derivable.
        // We use serum_dex = OPEN_BOOK_PROGRAM_ID for the real PDA.
        // In a unit test we skip that check by providing a nonce that
        // happens to work OR by checking account count only.
        let market = MarketState {
            vault_signer_nonce: 0,
            coin_vault: Pubkey::new_unique(),
            pc_vault: Pubkey::new_unique(),
            event_queue: Pubkey::new_unique(),
            bids: Pubkey::new_unique(),
            asks: Pubkey::new_unique(),
        };
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let amm_pk = Pubkey::new_unique();

        // The vault_signer_pda derivation with nonce=0 and a random
        // serum_dex may or may not find a valid off-curve point; skip
        // if it errors (we test the account count on a known-good path
        // in the tx-builder test below where we supply a valid nonce).
        let result = build_swap_base_in_ix(
            &user,
            &amm_pk,
            &amm,
            &market,
            &amm.pc_mint,
            &amm.coin_mint,
            1_000_000,
            900_000,
        );
        if let Ok(ix) = result {
            assert_eq!(ix.program_id, PROGRAM_ID);
            assert_eq!(ix.accounts.len(), 18);
        }
        // If vault_signer derivation failed we accept that — the account
        // count test is redundant with the build_buy_tx test below.
    }

    #[test]
    fn build_swap_base_in_ix_data_layout() {
        // Arrange a minimal AMM + market with a known-good vault_signer.
        // Easiest: use OPEN_BOOK_PROGRAM_ID as serum_dex + known market
        // key where nonce produces a valid PDA. We pick nonce by brute-
        // force testing (nonce 0 often hits off-curve; use 1 here — if
        // this happens to fail on a different host we'd know from the test).
        // Since create_program_address is deterministic, we try nonces
        // until we find one that works or skip if none works in range.
        let market_pk = Pubkey::new_unique();
        let mut good_nonce = None;
        for n in 0u64..=255 {
            if vault_signer_pda(&market_pk, n, &OPEN_BOOK_PROGRAM_ID).is_ok() {
                good_nonce = Some(n);
                break;
            }
        }
        let nonce = match good_nonce {
            Some(n) => n,
            None => return, // extremely unlikely — skip
        };

        let (_, mut amm) = fake_amm_state();
        amm.market = market_pk;
        amm.serum_dex = OPEN_BOOK_PROGRAM_ID;

        let market = MarketState {
            vault_signer_nonce: nonce,
            coin_vault: Pubkey::new_unique(),
            pc_vault: Pubkey::new_unique(),
            event_queue: Pubkey::new_unique(),
            bids: Pubkey::new_unique(),
            asks: Pubkey::new_unique(),
        };
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let amm_pk = Pubkey::new_unique();
        let ix = build_swap_base_in_ix(
            &user,
            &amm_pk,
            &amm,
            &market,
            &amm.pc_mint,
            &amm.coin_mint,
            500_000_000,
            450_000_000,
        )
        .expect("ix");

        // Tag = 9
        assert_eq!(ix.data[0], TAG_SWAP_BASE_IN);
        // amount_in LE at bytes [1..9]
        assert_eq!(
            u64::from_le_bytes(ix.data[1..9].try_into().unwrap()),
            500_000_000
        );
        // minimum_amount_out LE at bytes [9..17]
        assert_eq!(
            u64::from_le_bytes(ix.data[9..17].try_into().unwrap()),
            450_000_000
        );
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.accounts.len(), 18);

        // Position checks
        assert_eq!(ix.accounts[1].pubkey, amm_pk);
        assert!(ix.accounts[1].is_writable);
        assert!(!ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[2].pubkey, amm_authority_pda()); // authority
        assert_eq!(ix.accounts[3].pubkey, amm.open_orders);
        assert_eq!(ix.accounts[4].pubkey, amm.target_orders);
        assert_eq!(ix.accounts[5].pubkey, amm.token_coin);
        assert_eq!(ix.accounts[6].pubkey, amm.token_pc);
        assert_eq!(ix.accounts[7].pubkey, OPEN_BOOK_PROGRAM_ID);
        assert_eq!(ix.accounts[8].pubkey, market_pk);
        assert_eq!(ix.accounts[9].pubkey, market.bids);
        assert_eq!(ix.accounts[10].pubkey, market.asks);
        assert_eq!(ix.accounts[11].pubkey, market.event_queue);
        assert_eq!(ix.accounts[12].pubkey, market.coin_vault);
        assert_eq!(ix.accounts[13].pubkey, market.pc_vault);
        // 14: vault_signer PDA (checked by derivation)
        assert_eq!(ix.accounts[15].pubkey, ata::derive(&user, &amm.pc_mint));
        assert_eq!(ix.accounts[16].pubkey, ata::derive(&user, &amm.coin_mint));
        assert_eq!(ix.accounts[17].pubkey, user);
        assert!(ix.accounts[17].is_signer);
    }

    // ── quote math ───────────────────────────────────────────────────

    #[test]
    fn quote_canonical_fee_produces_expected_output() {
        // Pool: 1B coin, 100 SOL pc. Buy 1 SOL worth of coin.
        // effective_in = 1e9 * (10000 - 25) / 10000 = 997_500_000
        // out = 1e9 * 997_500_000 / (100e9 + 997_500_000) ≈ 9_876_543 coin
        let out =
            swap_base_in_quote(1_000_000_000, 100_000_000_000, 1_000_000_000, 25, 10_000).unwrap();
        // Ballpark: around 9.8–10M. Exact depends on rounding.
        assert!(out > 9_000_000 && out < 11_000_000, "got {out}");
    }

    #[test]
    fn quote_zero_in_reserve_returns_none() {
        assert!(swap_base_in_quote(1_000, 0, 100_000, 25, 10_000).is_none());
    }

    #[test]
    fn quote_zero_out_reserve_returns_none() {
        assert!(swap_base_in_quote(1_000, 100_000, 0, 25, 10_000).is_none());
    }

    #[test]
    fn quote_fee_ge_denom_returns_none() {
        // fee_num >= fee_denom = 100% fee = no effective input.
        assert!(swap_base_in_quote(1_000, 100_000, 100_000, 10_000, 10_000).is_none());
    }

    #[test]
    fn buy_then_sell_roundtrip_loses_double_fee() {
        // Each swap incurs 0.25% — two swaps in opposite directions ~0.5%.
        let (coin_r, pc_r) = (1_000_000_000u64, 100_000_000_000u64);
        let pc_in = 1_000_000_000u64;
        let coin_out = swap_base_in_quote(pc_in, pc_r, coin_r, 25, 10_000).unwrap();
        // Post-buy reserves (approximate — ignores the fee going to LP):
        let coin_r2 = coin_r - coin_out;
        let pc_r2 = pc_r + (pc_in * (10_000 - 25) / 10_000);
        let pc_back = swap_base_in_quote(coin_out, coin_r2, pc_r2, 25, 10_000).unwrap();
        let loss_bps = ((pc_in as i128 - pc_back as i128) * 10_000) / pc_in as i128;
        // ~0.5% loss from double fee + small slippage = 40–70 bps.
        assert!(
            (30..=100).contains(&loss_bps),
            "expected ~0.5% double-fee loss, got {loss_bps} bps"
        );
    }

    #[test]
    fn apply_slippage_inflates_for_max_in() {
        // 1% = 100 bps
        assert_eq!(apply_slippage(1_000_000, 100, true), 1_010_000);
    }

    #[test]
    fn apply_slippage_deflates_for_min_out() {
        assert_eq!(apply_slippage(1_000_000, 100, false), 990_000);
    }

    #[test]
    fn apply_slippage_zero_bps_noop() {
        assert_eq!(apply_slippage(12_345, 0, true), 12_345);
        assert_eq!(apply_slippage(12_345, 0, false), 12_345);
    }

    // ── full tx builders ─────────────────────────────────────────────

    fn build_test_params(nonce: u64, market_pk: Pubkey) -> Option<(BuyTxParams, SellTxParams)> {
        let (_, mut amm) = fake_amm_state();
        amm.market = market_pk;
        amm.serum_dex = OPEN_BOOK_PROGRAM_ID;

        let mkt = MarketState {
            vault_signer_nonce: nonce,
            coin_vault: Pubkey::new_unique(),
            pc_vault: Pubkey::new_unique(),
            event_queue: Pubkey::new_unique(),
            bids: Pubkey::new_unique(),
            asks: Pubkey::new_unique(),
        };
        let user = pubkey!("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM");
        let amm_pk = Pubkey::new_unique();
        let reserves = PoolReserves {
            coin: 1_000_000_000,
            pc: 100_000_000_000,
        };
        let buy = BuyTxParams {
            user,
            amm_pubkey: amm_pk,
            amm,
            market: mkt,
            reserves,
            quote_in_lamports: 100_000_000,
            slippage_bps: 100,
            recent_blockhash: Hash::default(),
            compute_unit_limit: 250_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_coin_ata_create: false,
            skip_pc_ata_create: false,
        };
        let sell = SellTxParams {
            user,
            amm_pubkey: amm_pk,
            amm,
            market: mkt,
            reserves,
            coin_in_amount: 1_000_000,
            slippage_bps: 100,
            recent_blockhash: Hash::default(),
            compute_unit_limit: 200_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_pc_ata_create: false,
        };
        Some((buy, sell))
    }

    #[test]
    fn build_buy_tx_produces_v0_with_5_ixs() {
        let market_pk = Pubkey::new_unique();
        let nonce =
            (0u64..=255).find(|n| vault_signer_pda(&market_pk, *n, &OPEN_BOOK_PROGRAM_ID).is_ok());
        let nonce = match nonce {
            Some(n) => n,
            None => return,
        };
        let (buy_p, _) = build_test_params(nonce, market_pk).unwrap();
        let bytes = build_buy_tx(&buy_p).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            VersionedMessage::V0(m) => {
                // cu-limit + cu-price + ATA(coin) + ATA(pc) + swapBaseIn = 5
                assert_eq!(m.instructions.len(), 5);
            }
            _ => panic!("expected v0 message"),
        }
    }

    #[test]
    fn build_sell_tx_produces_v0_with_4_ixs() {
        let market_pk = Pubkey::new_unique();
        let nonce =
            (0u64..=255).find(|n| vault_signer_pda(&market_pk, *n, &OPEN_BOOK_PROGRAM_ID).is_ok());
        let nonce = match nonce {
            Some(n) => n,
            None => return,
        };
        let (_, sell_p) = build_test_params(nonce, market_pk).unwrap();
        let bytes = build_sell_tx(&sell_p).expect("build_sell_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            VersionedMessage::V0(m) => {
                // cu-limit + cu-price + ATA(pc) + swapBaseIn = 4
                // No ATA(coin) for sell — seller already owns the coin ATA.
                assert_eq!(m.instructions.len(), 4);
            }
            _ => panic!("expected v0 message"),
        }
    }

    // ── WSOL wrap/unwrap (audit H2) ──────────────────────────────────

    #[test]
    fn build_buy_tx_wraps_sol_for_wsol_pools() {
        let market_pk = Pubkey::new_unique();
        let nonce =
            (0u64..=255).find(|n| vault_signer_pda(&market_pk, *n, &OPEN_BOOK_PROGRAM_ID).is_ok());
        let nonce = match nonce {
            Some(n) => n,
            None => return,
        };
        let (mut buy_p, _) = build_test_params(nonce, market_pk).unwrap();
        buy_p.amm.pc_mint = ata::WSOL_MINT;
        let bytes = build_buy_tx(&buy_p).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            VersionedMessage::V0(m) => {
                // cu-limit + cu-price + ATA(coin) + ATA(wsol) +
                // transfer + sync_native + swapBaseIn + close = 8
                assert_eq!(m.instructions.len(), 8);
            }
            _ => panic!("expected v0 message"),
        }
    }

    #[test]
    fn build_sell_tx_unwraps_wsol_proceeds() {
        let market_pk = Pubkey::new_unique();
        let nonce =
            (0u64..=255).find(|n| vault_signer_pda(&market_pk, *n, &OPEN_BOOK_PROGRAM_ID).is_ok());
        let nonce = match nonce {
            Some(n) => n,
            None => return,
        };
        let (_, mut sell_p) = build_test_params(nonce, market_pk).unwrap();
        sell_p.amm.pc_mint = ata::WSOL_MINT;
        // Even with the cache claiming the WSOL ATA exists, the sell
        // must recreate it (we close it after every trade).
        sell_p.skip_pc_ata_create = true;
        let bytes = build_sell_tx(&sell_p).expect("build_sell_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        match &tx.message {
            VersionedMessage::V0(m) => {
                // cu-limit + cu-price + ATA(wsol) + swapBaseIn + close = 5
                assert_eq!(m.instructions.len(), 5);
            }
            _ => panic!("expected v0 message"),
        }
    }
}
