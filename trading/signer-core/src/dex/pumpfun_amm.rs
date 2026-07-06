//! PumpFun-AMM (Pumpswap) — native instruction encoder.
//!
//! Post-graduation venue: once a classic PumpFun curve hits the
//! migration threshold (~85 SOL), liquidity moves to a Pumpswap
//! constant-product pool here.
//!
//! Rebuilt 2026-06-11 against the LIVE on-chain program (audit M1).
//! Verified against the on-chain Anchor IDL v0.1.0 + live mainnet
//! transactions (sigs pinned in the tests below).
//!
//! ## Wire shape
//!
//! - **Anchor discriminators** — same 8 bytes as classic
//!   (`buy = 0x66063d1201daebea`, `sell = 0x33e685a4017f83ad`); the
//!   *program id* (`pAMMBay…`) is what disambiguates them on chain.
//! - **Args** — `base_amount_out` + `max_quote_amount_in` for buy,
//!   `base_amount_in` + `min_quote_amount_out` for sell. Two `u64 LE`
//!   after the discriminator. Buys additionally accept an optional
//!   trailing `track_volume: OptionBool` byte which we omit.
//! - **Buy = 23 accounts / Sell = 21 accounts** (IDL order, see
//!   `buy_accounts` / `sell_accounts`). Live router transactions
//!   append 2-3 extra *remaining accounts* (front-end fee-share);
//!   those are optional and omitted here.
//!
//! ## PDAs (seeds verified against live txs in the tests)
//!
//!   pool                        = `["pool", index_le_u16, creator, base_mint, quote_mint]`
//!                                 — note the CREATOR seed; the old
//!                                 4-seed derivation never matched any
//!                                 live pool (audit M1).
//!   global_config               = `["global_config"]`
//!   coin_creator_vault_authority= `["creator_vault", pool.coin_creator]`
//!                                 (underscore — different string than
//!                                 classic's `"creator-vault"`)
//!   coin_creator_vault_ata      = ATA(authority, quote_mint, quote_token_program)
//!   global_volume_accumulator   = `["global_volume_accumulator"]` (buys)
//!   user_volume_accumulator     = `["user_volume_accumulator", user]` (buys)
//!   fee_config                  = `["fee_config", PROGRAM_ID]` on the
//!                                 pump fee program `pfeeUxB6…`
//!
//! ## Token-2022
//!
//! Live pools mix legacy + Token-2022 per side (audit M2): the base
//! mint is frequently Token-2022 while WSOL stays legacy. Each side's
//! token program is threaded through every ATA derivation and the
//! `base_token_program` / `quote_token_program` instruction accounts.
//!
//! ## WSOL wrap / unwrap (audit H2)
//!
//! When the quote side is WSOL, `build_buy_tx` funds the user's WSOL
//! ATA (system transfer + `SyncNative`) before the swap and closes the
//! ATA after it; `build_sell_tx` closes the ATA after the swap so
//! proceeds land as native SOL. Without this, buys sim-fail with
//! insufficient funds and sell proceeds strand invisibly as WSOL.

use crate::dex::tip::{self, TipProvider, TipSelector};
use crate::dex::{ata, compute_budget};
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;

/// PumpFun-AMM (Pumpswap) program id. Different from PumpFun classic.
pub const PROGRAM_ID: Pubkey = pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");

/// Pump fee program — owns the `fee_config` PDA (same program classic
/// uses).
pub const FEE_PROGRAM_ID: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

/// Protocol fee recipient — observed on every live PumpSwap swap we
/// sampled (2026-06-11) and also accepted by PumpFun classic. The
/// on-chain `GlobalConfig` carries a recipient rotation; this member
/// is verified-live.
pub const FEE_RECIPIENT: Pubkey = pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");

/// Buyback fee recipient — `buyback_fee_recipients[0]` of the shared
/// fee rotation (decoded live 2026-06-11 from the pump `Global`
/// account; the same set serves PumpSwap). Observed verbatim on live
/// PumpSwap buy sig `5Ls8BMWGf3JC1VF3hqn6uxjy4qYDk6c8j3WptJuRT5iYu234u3vKvyvdWgD2PeJP76MzGyWuymRanpfiJ1Hqyt3j`
/// (account 23) and sell sig `3bmEMeqcfKQ4A8B5JeurpQKazUQWRsNP9QnmT27Wqb3sp3rfvZfsSUbUKGJHyVdaJNXTqtELTYDKZHwPBguEasHi`
/// (account 22). The deployed handlers REQUIRE this + its quote-mint
/// ATA as trailing accounts even though the on-chain IDL doesn't
/// declare them (2026 buyback feature, post-IDL).
pub const BUYBACK_FEE_RECIPIENT: Pubkey = pubkey!("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD");

/// WSOL mint — quote side of every PumpSwap pool that graduated from
/// a classic PumpFun curve (the curve was always SOL-paired).
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

/// Anchor discriminators — same as classic, program id disambiguates.
pub const DISCRIM_BUY: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
pub const DISCRIM_SELL: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Total swap fee in bps used for QUOTE SIZING only (lp + protocol +
/// coin-creator). The live fee is volume-tiered via `fee_config`; we
/// keep the conservative historical 100 bps so the on-chain min-out
/// bound never over-promises. Worst case the realised price is
/// slightly better than quoted.
pub const PROTOCOL_FEE_BPS: u64 = 100;

/// `["pool", index_le_u16, creator, base_mint, quote_mint]` PDA.
/// `creator` is the POOL creator (canonical graduated pools are
/// created by the pump migration authority; permissionless pools by
/// arbitrary wallets — read it from the decoded `Pool` account).
pub fn pool_pda(creator: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey, index: u16) -> Pubkey {
    let index_bytes = index.to_le_bytes();
    let (pda, _bump) = Pubkey::find_program_address(
        &[
            b"pool",
            &index_bytes,
            creator.as_ref(),
            base_mint.as_ref(),
            quote_mint.as_ref(),
        ],
        &PROGRAM_ID,
    );
    pda
}

/// PumpFun CLASSIC program id — hosts the per-mint migration
/// authority PDA that creates every canonical graduated pool.
pub const PUMP_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

/// The migration `pool_authority` PDA for a mint —
/// `["pool-authority", mint]` on the CLASSIC pump program. This PDA
/// is the `creator` of the canonical graduated pool (verified against
/// the classic program's `migrate` instruction IDL + live pool
/// accounts; see tests).
pub fn canonical_pool_authority_pda(mint: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &PUMP_PROGRAM_ID);
    pda
}

/// The canonical post-graduation pool for `mint`:
/// `pool_pda(pool_authority(mint), mint, WSOL, 0)`.
pub fn canonical_pool_pda(mint: &Pubkey) -> Pubkey {
    pool_pda(&canonical_pool_authority_pda(mint), mint, &WSOL_MINT, 0)
}

/// `["global_config"]` PDA — singleton, holds fee config + admin.
pub fn global_config_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"global_config"], &PROGRAM_ID);
    pda
}

/// `["__event_authority"]` PDA — Anchor CPI-event-relay convention,
/// same string as classic.
pub fn event_authority_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID);
    pda
}

/// `["creator_vault", coin_creator]` PDA — authority of the coin
/// creator's fee ATA. NOTE: underscore, not the hyphen classic uses.
pub fn coin_creator_vault_authority_pda(coin_creator: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], &PROGRAM_ID);
    pda
}

/// The coin creator's fee ATA — quote-mint ATA owned by the
/// creator-vault authority PDA, derived under the QUOTE token program.
pub fn coin_creator_vault_ata(
    coin_creator: &Pubkey,
    quote_mint: &Pubkey,
    quote_token_program: &Pubkey,
) -> Pubkey {
    let authority = coin_creator_vault_authority_pda(coin_creator);
    ata::derive_with_program(&authority, quote_mint, quote_token_program)
}

/// `["global_volume_accumulator"]` PDA (buys only).
pub fn global_volume_accumulator_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"global_volume_accumulator"], &PROGRAM_ID);
    pda
}

/// `["user_volume_accumulator", user]` PDA (buys only).
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &PROGRAM_ID);
    pda
}

/// `["fee_config", PROGRAM_ID]` PDA on the pump fee program.
pub fn fee_config_pda() -> Pubkey {
    let (pda, _bump) =
        Pubkey::find_program_address(&[b"fee_config", PROGRAM_ID.as_ref()], &FEE_PROGRAM_ID);
    pda
}

/// `["pool-v2", base_mint]` PDA — the cashback-era pool extension
/// account (2026 upgrade). The deployed buy/sell handlers REQUIRE it
/// as a trailing account (uninitialized for non-cashback coins is
/// fine); omitting it makes the fee math read garbage indices and
/// revert with `Overflow` (6023). Verified against live sell tx
/// `3bmEMeqcfKQ4…` account 21.
pub fn pool_v2_pda(base_mint: &Pubkey) -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &PROGRAM_ID);
    pda
}

/// Live reserves snapshot. Caller fetches via
/// `getTokenAccountBalance` on the pool's base + quote vaults (their
/// addresses are stored in the `Pool` account).
#[derive(Debug, Clone, Copy)]
pub struct PoolReserves {
    pub base: u64,
    pub quote: u64,
}

/// Everything the instruction builders need to address a pool.
/// Assembled by the route selector from the decoded [`PoolAccount`]
/// plus the two mint owners (token programs).
#[derive(Debug, Clone, Copy)]
pub struct PoolKeys {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Owner program of `base_mint` (legacy or Token-2022).
    pub base_token_program: Pubkey,
    /// Owner program of `quote_mint` (WSOL = legacy).
    pub quote_token_program: Pubkey,
    /// `Pool.coin_creator` — drives the creator-vault accounts.
    pub coin_creator: Pubkey,
}

/// Build a `buy` instruction (live 25-account layout) — spend quote
/// (typically WSOL), receive base.
pub fn build_buy_ix(
    user: &Pubkey,
    k: &PoolKeys,
    base_amount_out: u64,
    max_quote_amount_in: u64,
) -> Instruction {
    let mut accounts = common_accounts(user, k);
    // Buys append: gva · uva · fee_config · fee_program.
    accounts.push(AccountMeta::new_readonly(
        global_volume_accumulator_pda(),
        false,
    )); // 19
    accounts.push(AccountMeta::new(user_volume_accumulator_pda(user), false)); // 20
    accounts.push(AccountMeta::new_readonly(fee_config_pda(), false)); // 21
    accounts.push(AccountMeta::new_readonly(FEE_PROGRAM_ID, false)); // 22
    buyback_tail(&mut accounts, k); // 23, 24
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: encode_args(DISCRIM_BUY, base_amount_out, max_quote_amount_in),
    }
}

/// Build a `sell` instruction (live 23-account layout) — burn base,
/// receive quote.
pub fn build_sell_ix(
    user: &Pubkey,
    k: &PoolKeys,
    base_amount_in: u64,
    min_quote_amount_out: u64,
) -> Instruction {
    let mut accounts = common_accounts(user, k);
    // Sells append: fee_config · fee_program (no volume accumulators).
    accounts.push(AccountMeta::new_readonly(fee_config_pda(), false)); // 19
    accounts.push(AccountMeta::new_readonly(FEE_PROGRAM_ID, false)); // 20
    buyback_tail(&mut accounts, k); // 21, 22
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data: encode_args(DISCRIM_SELL, base_amount_in, min_quote_amount_out),
    }
}

/// Trailing accounts the DEPLOYED handlers require beyond the on-chain
/// IDL (2026 cashback/buyback upgrade): the per-mint `pool_v2`
/// extension PDA (readonly, may be uninitialized), the buyback fee
/// recipient (readonly — fees flow to its quote-mint ATA), and that
/// quote ATA (writable). Verified against live sell tx
/// `3bmEMeqcfKQ4…` accounts 21..=23 and by live simulation; omitting
/// them reverts with `Overflow` (6023).
fn buyback_tail(accounts: &mut Vec<AccountMeta>, k: &PoolKeys) {
    accounts.push(AccountMeta::new_readonly(pool_v2_pda(&k.base_mint), false));
    accounts.push(AccountMeta::new_readonly(BUYBACK_FEE_RECIPIENT, false));
    accounts.push(AccountMeta::new(
        ata::derive_with_program(
            &BUYBACK_FEE_RECIPIENT,
            &k.quote_mint,
            &k.quote_token_program,
        ),
        false,
    ));
}

/// Accounts 0..=18 shared by `buy` + `sell` (IDL order).
fn common_accounts(user: &Pubkey, k: &PoolKeys) -> Vec<AccountMeta> {
    let user_base = ata::derive_with_program(user, &k.base_mint, &k.base_token_program);
    let user_quote = ata::derive_with_program(user, &k.quote_mint, &k.quote_token_program);
    let pool_base = ata::derive_with_program(&k.pool, &k.base_mint, &k.base_token_program);
    let pool_quote = ata::derive_with_program(&k.pool, &k.quote_mint, &k.quote_token_program);
    let protocol_fee_token_account =
        ata::derive_with_program(&FEE_RECIPIENT, &k.quote_mint, &k.quote_token_program);
    let ccva = coin_creator_vault_ata(&k.coin_creator, &k.quote_mint, &k.quote_token_program);
    let ccva_authority = coin_creator_vault_authority_pda(&k.coin_creator);
    vec![
        AccountMeta::new(k.pool, false),                                  // 0
        AccountMeta::new(*user, true),                                    // 1 signer+writable
        AccountMeta::new_readonly(global_config_pda(), false),            // 2
        AccountMeta::new_readonly(k.base_mint, false),                    // 3
        AccountMeta::new_readonly(k.quote_mint, false),                   // 4
        AccountMeta::new(user_base, false),                               // 5
        AccountMeta::new(user_quote, false),                              // 6
        AccountMeta::new(pool_base, false),                               // 7
        AccountMeta::new(pool_quote, false),                              // 8
        AccountMeta::new_readonly(FEE_RECIPIENT, false),                  // 9
        AccountMeta::new(protocol_fee_token_account, false),              // 10
        AccountMeta::new_readonly(k.base_token_program, false),           // 11
        AccountMeta::new_readonly(k.quote_token_program, false),          // 12
        AccountMeta::new_readonly(solana_sdk::system_program::ID, false), // 13
        AccountMeta::new_readonly(ata::ASSOCIATED_TOKEN_PROGRAM_ID, false), // 14
        AccountMeta::new_readonly(event_authority_pda(), false),          // 15
        AccountMeta::new_readonly(PROGRAM_ID, false),                     // 16
        AccountMeta::new(ccva, false),                                    // 17
        AccountMeta::new_readonly(ccva_authority, false),                 // 18
    ]
}

fn encode_args(disc: [u8; 8], amount: u64, bound: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&bound.to_le_bytes());
    data
}

// ── quote math (constant-product with fees) ──────────────────────

/// Compute the base-token output for a buy of `quote_in` quote-tokens,
/// against a pool with `(base_reserve, quote_reserve)`. Applies a
/// `PROTOCOL_FEE_BPS` cut on the input side.
///
/// Math: `base_out = base * (quote_in_after_fee) / (quote + quote_in_after_fee)`
///
/// Returns `None` on overflow or zero reserves.
pub fn buy_quote(quote_in: u64, base_reserve: u64, quote_reserve: u64) -> Option<u64> {
    if base_reserve == 0 || quote_reserve == 0 {
        return None;
    }
    let quote_in_after_fee =
        (quote_in as u128).checked_mul((10_000 - PROTOCOL_FEE_BPS) as u128)? / 10_000;
    let numerator = (base_reserve as u128).checked_mul(quote_in_after_fee)?;
    let denominator = (quote_reserve as u128).checked_add(quote_in_after_fee)?;
    let out = numerator.checked_div(denominator)?;
    if out > u64::MAX as u128 {
        return None;
    }
    Some(out as u64)
}

/// Compute the quote-token output for a sell of `base_in` base tokens.
/// Symmetric to `buy_quote` — fee applied on the *output* side.
pub fn sell_quote(base_in: u64, base_reserve: u64, quote_reserve: u64) -> Option<u64> {
    if base_reserve == 0 || quote_reserve == 0 {
        return None;
    }
    let numerator = (quote_reserve as u128).checked_mul(base_in as u128)?;
    let denominator = (base_reserve as u128).checked_add(base_in as u128)?;
    let gross_out = numerator.checked_div(denominator)?;
    let net_out = gross_out.checked_mul((10_000 - PROTOCOL_FEE_BPS) as u128)? / 10_000;
    if net_out > u64::MAX as u128 {
        return None;
    }
    Some(net_out as u64)
}

/// Apply slippage tolerance — `up=true` inflates (for max-in bounds),
/// `up=false` deflates (for min-out bounds).
pub fn apply_slippage(quote: u64, bps: u16, up: bool) -> u64 {
    let q = quote as u128;
    let adj = q * bps as u128 / 10_000;
    if up {
        (q + adj).min(u64::MAX as u128) as u64
    } else {
        q.saturating_sub(adj) as u64
    }
}

// ── Pool account decoder ─────────────────────────────────────────

/// Anchor account discriminator for `Pool` = first 8 bytes of
/// `sha256("account:Pool")`. Required prefix on every pool account
/// data blob.
pub const POOL_ACCOUNT_DISCRIM: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];

/// Decoded `Pool` account state.
///
/// Field offsets (after the 8-byte discriminator):
///   u8         pool_bump
///   u16 LE     index
///   Pubkey 32  creator
///   Pubkey 32  base_mint
///   Pubkey 32  quote_mint
///   Pubkey 32  lp_mint
///   Pubkey 32  pool_base_token_account
///   Pubkey 32  pool_quote_token_account
///   u64 LE     lp_supply
///   Pubkey 32  coin_creator
///
/// Live accounts append `is_mayhem_mode` / `is_cashback_coin` (and
/// future fields) past `coin_creator`; we read the leading 243 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolAccount {
    pub pool_bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub lp_supply: u64,
    pub coin_creator: Pubkey,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("data too short ({0} bytes, expected ≥243)")]
    TooShort(usize),
    #[error("discriminator mismatch — not a PumpSwap Pool account")]
    BadDiscriminator,
}

/// Decode the on-chain pool account blob. Caller fetches via
/// `getAccountInfo` and passes `account.data`. We read the first 243
/// bytes; trailing fields/padding are ignored.
pub fn decode_pool(data: &[u8]) -> Result<PoolAccount, DecodeError> {
    const MIN_LEN: usize = 8 + 1 + 2 + 32 * 6 + 8 + 32;
    if data.len() < MIN_LEN {
        return Err(DecodeError::TooShort(data.len()));
    }
    if data[..8] != POOL_ACCOUNT_DISCRIM {
        return Err(DecodeError::BadDiscriminator);
    }
    let mut o = 8usize;
    let pool_bump = data[o];
    o += 1;
    let index = u16::from_le_bytes(data[o..o + 2].try_into().unwrap());
    o += 2;
    let read_pk = |off: usize| -> Pubkey {
        let mut b = [0u8; 32];
        b.copy_from_slice(&data[off..off + 32]);
        Pubkey::new_from_array(b)
    };
    let creator = read_pk(o);
    o += 32;
    let base_mint = read_pk(o);
    o += 32;
    let quote_mint = read_pk(o);
    o += 32;
    let lp_mint = read_pk(o);
    o += 32;
    let pool_base_token_account = read_pk(o);
    o += 32;
    let pool_quote_token_account = read_pk(o);
    o += 32;
    let lp_supply = u64::from_le_bytes(data[o..o + 8].try_into().unwrap());
    o += 8;
    let coin_creator = read_pk(o);
    Ok(PoolAccount {
        pool_bump,
        index,
        creator,
        base_mint,
        quote_mint,
        lp_mint,
        pool_base_token_account,
        pool_quote_token_account,
        lp_supply,
        coin_creator,
    })
}

// ── full swap-tx builders (unsigned) ─────────────────────────────

#[derive(Debug, Clone)]
pub struct BuyTxParams {
    pub user: Pubkey,
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Owner program of the base mint (legacy or Token-2022).
    pub base_token_program: Pubkey,
    /// Owner program of the quote mint (WSOL = legacy).
    pub quote_token_program: Pubkey,
    /// `Pool.coin_creator` from the decoded pool account.
    pub coin_creator: Pubkey,
    /// Quote token (typically WSOL) amount the user is willing to
    /// spend — same shape as classic's `sol_in_lamports`.
    pub quote_in_amount: u64,
    pub slippage_bps: u16,
    pub reserves: PoolReserves,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    /// Submit-provider tip. `None` → byte-identical to the pre-tip RPC
    /// path. `Falcon`/`Jito` inject an in-tx System `transfer` to a
    /// provider tip account. See [`crate::dex::tip`].
    pub tip_provider: TipProvider,
    pub tip_lamports: u64,
    pub tip_selector: TipSelector,
    /// Suppress the `CreateIdempotent` instruction for `base_mint`'s ATA
    /// when the caller has confirmed it already exists (via `AtaCache`).
    pub skip_base_ata_create: bool,
    /// Suppress the quote-ATA create. IGNORED when `quote_mint` is
    /// WSOL — the wrap sequence always (re)creates the WSOL ATA
    /// because the builders close it after every swap.
    pub skip_quote_ata_create: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildTxError {
    #[error("buy_quote returned None (zero reserves / overflow)")]
    QuoteUnavailable,
    #[error("v0 message compile failed: {0}")]
    Compile(String),
    #[error("bincode serialise failed: {0}")]
    Bincode(String),
    #[error("tip injection failed: {0}")]
    Tip(#[from] tip::TipError),
}

/// Append the provider-tip `transfer` to `ixs` when requested. Warns if
/// the tip ≥ the trade budget (still sent — operator opted in). Shared
/// by buy + sell. `trade_lamports == 0` for sells (no SOL-in budget).
fn push_tip_ix(
    ixs: &mut Vec<Instruction>,
    user: &Pubkey,
    provider: TipProvider,
    tip_lamports: u64,
    trade_lamports: u64,
    selector: TipSelector,
) -> Result<(), BuildTxError> {
    if let Some((ix, used)) = tip::tip_transfer_ix(user, provider, tip_lamports, selector)? {
        if used >= trade_lamports && trade_lamports > 0 {
            tracing::warn!(
                tip_lamports = used,
                trade_lamports,
                "provider tip ≥ trade size — sending anyway (operator opted in)"
            );
        }
        ixs.push(ix);
    }
    Ok(())
}

/// Build an unsigned v0 VersionedTransaction for a PumpSwap buy.
///
/// WSOL-quoted pools (the graduated-PumpFun default): the builder
/// wraps native SOL just-in-time — create WSOL ATA (idempotent) →
/// fund with `max_quote_in` lamports → `SyncNative` → swap → close
/// WSOL ATA (returns leftover + rent as native SOL). Audit H2.
pub fn build_buy_tx(p: &BuyTxParams) -> Result<Vec<u8>, BuildTxError> {
    let base_out = buy_quote(p.quote_in_amount, p.reserves.base, p.reserves.quote)
        .ok_or(BuildTxError::QuoteUnavailable)?;
    // Slippage on the *quote* side — input bound widens on a buy.
    let max_quote_in = apply_slippage(p.quote_in_amount, p.slippage_bps, true);
    // Slippage on the *base* side: ship the conservative min-out so a
    // small reserve drift between route lookup + submit doesn't revert.
    let min_base_out = apply_slippage(base_out, p.slippage_bps, false);

    let keys = PoolKeys {
        pool: p.pool,
        base_mint: p.base_mint,
        quote_mint: p.quote_mint,
        base_token_program: p.base_token_program,
        quote_token_program: p.quote_token_program,
        coin_creator: p.coin_creator,
    };
    let wrap_sol = p.quote_mint == WSOL_MINT;
    let user_quote_ata = ata::derive_with_program(&p.user, &p.quote_mint, &p.quote_token_program);

    let mut ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
    ];
    if !p.skip_base_ata_create {
        ixs.push(ata::create_idempotent_ix_with_program(
            &p.user,
            &p.user,
            &p.base_mint,
            &p.base_token_program,
        ));
    }
    if wrap_sol {
        // Wrap: ATA create (always — we close it below), fund, sync.
        ixs.push(ata::create_idempotent_ix_with_program(
            &p.user,
            &p.user,
            &p.quote_mint,
            &p.quote_token_program,
        ));
        ixs.push(ata::system_transfer_ix(
            &p.user,
            &user_quote_ata,
            max_quote_in,
        ));
        ixs.push(ata::sync_native_ix(&user_quote_ata));
    } else if !p.skip_quote_ata_create {
        ixs.push(ata::create_idempotent_ix_with_program(
            &p.user,
            &p.user,
            &p.quote_mint,
            &p.quote_token_program,
        ));
    }
    ixs.push(build_buy_ix(&p.user, &keys, min_base_out, max_quote_in));
    if wrap_sol {
        // Unwrap leftovers (slippage headroom we funded but the swap
        // didn't consume) + rent back to native SOL.
        ixs.push(ata::close_account_ix(&user_quote_ata, &p.user, &p.user));
    }
    push_tip_ix(
        &mut ixs,
        &p.user,
        p.tip_provider,
        p.tip_lamports,
        p.quote_in_amount,
        p.tip_selector,
    )?;

    compile_tx(&p.user, ixs, p.recent_blockhash)
}

#[derive(Debug, Clone)]
pub struct SellTxParams {
    pub user: Pubkey,
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Owner program of the base mint (legacy or Token-2022).
    pub base_token_program: Pubkey,
    /// Owner program of the quote mint (WSOL = legacy).
    pub quote_token_program: Pubkey,
    /// `Pool.coin_creator` from the decoded pool account.
    pub coin_creator: Pubkey,
    pub base_in_amount: u64,
    pub slippage_bps: u16,
    pub reserves: PoolReserves,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro_lamports: u64,
    /// Submit-provider tip — see [`BuyTxParams::tip_provider`]. Exits
    /// tip too; tip logic never blocks a sell.
    pub tip_provider: TipProvider,
    pub tip_lamports: u64,
    pub tip_selector: TipSelector,
    /// Suppress the quote-ATA create. IGNORED when `quote_mint` is
    /// WSOL — see `BuyTxParams::skip_quote_ata_create`.
    pub skip_quote_ata_create: bool,
}

/// Build an unsigned v0 tx for a PumpSwap sell. WSOL-quoted pools get
/// the proceeds unwrapped to native SOL via `CloseAccount` (audit H2).
pub fn build_sell_tx(p: &SellTxParams) -> Result<Vec<u8>, BuildTxError> {
    let quote_out = sell_quote(p.base_in_amount, p.reserves.base, p.reserves.quote)
        .ok_or(BuildTxError::QuoteUnavailable)?;
    let min_quote_out = apply_slippage(quote_out, p.slippage_bps, false);

    let keys = PoolKeys {
        pool: p.pool,
        base_mint: p.base_mint,
        quote_mint: p.quote_mint,
        base_token_program: p.base_token_program,
        quote_token_program: p.quote_token_program,
        coin_creator: p.coin_creator,
    };
    let wrap_sol = p.quote_mint == WSOL_MINT;
    let user_quote_ata = ata::derive_with_program(&p.user, &p.quote_mint, &p.quote_token_program);

    let mut ixs = vec![
        compute_budget::set_compute_unit_limit(p.compute_unit_limit),
        compute_budget::set_compute_unit_price(p.compute_unit_price_micro_lamports),
    ];
    if wrap_sol || !p.skip_quote_ata_create {
        // The proceeds receiver must exist. For WSOL we ALWAYS create
        // (idempotent) because the previous trade closed the ATA.
        ixs.push(ata::create_idempotent_ix_with_program(
            &p.user,
            &p.user,
            &p.quote_mint,
            &p.quote_token_program,
        ));
    }
    ixs.push(build_sell_ix(
        &p.user,
        &keys,
        p.base_in_amount,
        min_quote_out,
    ));
    if wrap_sol {
        // Unwrap the SOL proceeds so they show up as native balance.
        ixs.push(ata::close_account_ix(&user_quote_ata, &p.user, &p.user));
    }
    push_tip_ix(
        &mut ixs,
        &p.user,
        p.tip_provider,
        p.tip_lamports,
        0,
        p.tip_selector,
    )?;

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
    let bytes = bincode::serialize(&unsigned).map_err(|e| BuildTxError::Bincode(e.to_string()))?;
    tip::check_tx_size(&bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    // ─────────────────────────────────────────────────────────────
    // LIVE MAINNET FIXTURES (fetched 2026-06-11 via public RPC)
    //
    // Sell tx sig (top-level, simple wallet):
    //   59BopXsRmm9ts44nEru83pBeJe3t6HGGNqVprUCUx83kZg1Vs82SHhEKVFC4Z9DdyqjSgN4fzrUZqc52YXReyNcF
    //   pool  EDvTpqvysRhQogZFGi2rNQrFzadcZfEAcHq7T1Q4koab
    //   base  7BdfmAP5gUAwWAj78Pn3axyLBU7aa6HC8gpfL81Hpump (Token-2022)
    //   quote WSOL (legacy token program)
    //   user  ron7XERDF3HNTxsPtoPiXEZ8psVaqSkAs3Twj1yxV5x
    // Buy tx sig:
    //   5Ls8BMWGf3JC1VF3hqn6uxjy4qYDk6c8j3WptJuRT5iYu234u3vKvyvdWgD2PeJP76MzGyWuymRanpfiJ1Hqyt3j
    //   (index-1 pool 2cvg5LGV…, WSOL base / Token-2022 quote —
    //   used here to prove index + creator participate in the pool PDA)
    // ─────────────────────────────────────────────────────────────

    /// `getAccountInfo` data prefix (300 bytes) of live pool
    /// EDvTpqvysRhQogZFGi2rNQrFzadcZfEAcHq7T1Q4koab (301 on chain; the
    /// decoder reads the leading 243).
    const LIVE_POOL_B64: &str = "8ZptBBGxbbz6AADider/XJszZUm+W0DGgDCBe3d3g4KIAix8V6dlnU4FS1vfZAAy6c1MEkCOX+Y+SHUOmfb6K+kLe5V7vHOn6fcPBpuIV/6rgYT7aH9jRhjANdrEOdwa6ztVmKDwAAAAAAEQX9h2ooFPJ83Z8Fa3hS+FJXuw0bkph2y6mCNBJLvWDlNqdMrQ4R4B6k2WYKnpn7KsuOowKyD4gHzf7T5ir489rHPQR3oiQ/2Szgf/7ebCw6XWFy2CC1BENVz5rFwnXUx9RmtZ0AMAAO9bA+PElf4jsDEiP0gizMfi2Qfrav2fmJxBcM2FB2NEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    /// `getAccountInfo` data (245 bytes) of live pool
    /// 2cvg5LGVh8Af1c2jTEoNa9G9StswqbMxcMkyesvr8Huk — an INDEX-1,
    /// permissionless pool (WSOL base / Token-2022 quote).
    const LIVE_POOL_IDX1_B64: &str = "8ZptBBGxbbz8AQAYPuIXL6D0fEHXcRlhzrVAgsdNsKbpQAwomgKhbgBhKgabiFf+q4GE+2h/Y0YYwDXaxDncGus7VZig8AAAAAABwAiYpH4Adu8WjAFZJpLlEziL9dCBkiKOPdbeGcWMQhgNUh4qAY2RH2aMOdJ/hP6HEr4eudSrBWbWqzvtcEEc54i6GnMaTlPrCRep66gfF3jT9FUvbooMIYl6u8bj1njQi3TZZdQg/ttSh5TQhWlps3XRAePr/o7WXhyIkBVa0/5kAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    const LIVE_POOL: Pubkey = pubkey!("EDvTpqvysRhQogZFGi2rNQrFzadcZfEAcHq7T1Q4koab");
    const LIVE_POOL_IDX1: Pubkey = pubkey!("2cvg5LGVh8Af1c2jTEoNa9G9StswqbMxcMkyesvr8Huk");
    const LIVE_BASE: Pubkey = pubkey!("7BdfmAP5gUAwWAj78Pn3axyLBU7aa6HC8gpfL81Hpump");
    const LIVE_USER: Pubkey = pubkey!("ron7XERDF3HNTxsPtoPiXEZ8psVaqSkAs3Twj1yxV5x");

    fn live_pool() -> PoolAccount {
        let data = base64::engine::general_purpose::STANDARD
            .decode(LIVE_POOL_B64)
            .unwrap();
        decode_pool(&data).expect("live pool decodes")
    }

    fn live_keys() -> PoolKeys {
        let pool = live_pool();
        PoolKeys {
            pool: LIVE_POOL,
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            base_token_program: ata::TOKEN_2022_PROGRAM_ID,
            quote_token_program: ata::TOKEN_PROGRAM_ID,
            coin_creator: pool.coin_creator,
        }
    }

    // ── discriminators + PDAs ────────────────────────────────────

    #[test]
    fn discriminators_match_classic_decoder() {
        assert_eq!(
            DISCRIM_BUY,
            [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]
        );
        assert_eq!(
            DISCRIM_SELL,
            [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]
        );
    }

    #[test]
    fn global_config_pda_matches_live() {
        // Account 2 in every live swap sampled.
        assert_eq!(
            global_config_pda(),
            pubkey!("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw")
        );
    }

    #[test]
    fn event_authority_pda_matches_live() {
        assert_eq!(
            event_authority_pda(),
            pubkey!("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR")
        );
    }

    #[test]
    fn fee_config_pda_matches_live() {
        assert_eq!(
            fee_config_pda(),
            pubkey!("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx")
        );
    }

    #[test]
    fn pool_pda_with_creator_seed_matches_live_pool() {
        // THE audit-M1 fix: the pool PDA includes the CREATOR. Decode
        // the live pool account and re-derive its own address.
        let pool = live_pool();
        assert_eq!(pool.index, 0);
        assert_eq!(pool.base_mint, LIVE_BASE);
        assert_eq!(pool.quote_mint, WSOL_MINT);
        assert_eq!(
            pool_pda(&pool.creator, &pool.base_mint, &pool.quote_mint, pool.index),
            LIVE_POOL
        );
    }

    #[test]
    fn canonical_pool_authority_matches_live_pool_creator() {
        // The canonical graduated pool's creator is the classic
        // program's per-mint migration PDA ["pool-authority", mint].
        let pool = live_pool();
        assert_eq!(
            canonical_pool_authority_pda(&LIVE_BASE),
            pool.creator,
            "creator of the live canonical pool must be the migration PDA"
        );
        // End-to-end: mint → canonical pool address, no on-chain data.
        assert_eq!(canonical_pool_pda(&LIVE_BASE), LIVE_POOL);
    }

    #[test]
    fn pool_pda_matches_live_index1_pool() {
        // Permissionless pool at index 1 (WSOL base / Token-2022
        // quote) — proves the index seed too.
        let data = base64::engine::general_purpose::STANDARD
            .decode(LIVE_POOL_IDX1_B64)
            .unwrap();
        let pool = decode_pool(&data).expect("decode idx1 pool");
        assert_eq!(pool.index, 1);
        assert_eq!(pool.base_mint, WSOL_MINT);
        assert_eq!(
            pool_pda(&pool.creator, &pool.base_mint, &pool.quote_mint, pool.index),
            LIVE_POOL_IDX1
        );
    }

    #[test]
    fn coin_creator_vault_pdas_match_live() {
        // Live sell accounts 17 + 18.
        let pool = live_pool();
        assert_eq!(
            coin_creator_vault_authority_pda(&pool.coin_creator),
            pubkey!("FYheReGqB2aoFbvnD8haJhwrAPr8mTRE9GDVNY3rDhBj")
        );
        assert_eq!(
            coin_creator_vault_ata(&pool.coin_creator, &WSOL_MINT, &ata::TOKEN_PROGRAM_ID),
            pubkey!("BeP9SP9PX6aWeB6FuwQ4fa89ZKXpNZaS8HCdQnf45LDi")
        );
    }

    #[test]
    fn volume_accumulator_pdas_match_live_buy() {
        // Live buy sig 5Ls8BMWG… accounts 19 + 20.
        assert_eq!(
            global_volume_accumulator_pda(),
            pubkey!("C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw")
        );
        assert_eq!(
            user_volume_accumulator_pda(&pubkey!("HMfNZJckBdqwhfWi59ahJZ6aMZDFdz3EyU1JHuBHwiNM")),
            pubkey!("8K2jUYsL16y1FLxMzTZRG79qpywn6BY6XUTvxzhkefZL")
        );
    }

    // ── full live-layout pinning: sell (24 accounts) ──

    #[test]
    fn build_sell_ix_matches_live_tx_layout() {
        // Pin the COMPLETE account list of live sell sig
        // 3bmEMeqcfKQ4A8B5JeurpQKazUQWRsNP9QnmT27Wqb3sp3rfvZfsSUbUKGJHyVdaJNXTqtELTYDKZHwPBguEasHi
        // — all 24 accounts match verbatim, including the 2026
        // cashback-era tail [pool_v2 · buyback_fee_recipient ·
        // buyback quote ATA].
        let ix = build_sell_ix(&LIVE_USER, &live_keys(), 1, 1);
        let expected: [(&str, bool); 24] = [
            ("EDvTpqvysRhQogZFGi2rNQrFzadcZfEAcHq7T1Q4koab", true), // pool
            ("ron7XERDF3HNTxsPtoPiXEZ8psVaqSkAs3Twj1yxV5x", true),  // user
            ("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw", false), // global_config
            ("7BdfmAP5gUAwWAj78Pn3axyLBU7aa6HC8gpfL81Hpump", false), // base_mint
            ("So11111111111111111111111111111111111111112", false), // quote_mint
            ("LPRJXfnGvE9TLYsas7YAhAJNpJVGU7wC33iEVGGPofE", true),  // user_base (2022 ATA)
            ("FG2tJqTMvaQDY1CxBakiQpktuFT2KwJkohP7pnRNCDfi", true), // user_quote
            ("6cczP83mihs9PNpfVAaPALaBP3FNh15FMrRvGprwVFja", true), // pool_base
            ("CcBYugEMNvPWkaM6deGkcdAJK7ipENwf3MQgykiyJ2tK", true), // pool_quote
            ("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV", false), // protocol_fee_recipient
            ("94qWNrtmfn42h3ZjUZwWvK1MEo9uVmmrBPd2hpNjYDjb", true), // pfr_token_account
            ("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", false), // base_token_program (2022)
            ("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", false), // quote_token_program
            ("11111111111111111111111111111111", false),            // system
            ("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", false), // ata program
            ("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR", false), // event_authority
            ("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA", false), // program
            ("BeP9SP9PX6aWeB6FuwQ4fa89ZKXpNZaS8HCdQnf45LDi", true), // coin_creator_vault_ata
            ("FYheReGqB2aoFbvnD8haJhwrAPr8mTRE9GDVNY3rDhBj", false), // ccva_authority
            ("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx", false), // fee_config
            ("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ", false), // fee_program
            ("FGfL9sVbxWkPEErAgmHbVBoww4fW94CQGbf8RENj4EHK", false), // pool_v2 (uninitialized)
            ("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD", false), // buyback_fee_recipient
            ("HjQjngTDqoHE6aaGhUqfz9aQ7WZcBRjy5xB8PScLSr8i", true), // buyback WSOL ATA
        ];
        assert_eq!(ix.accounts.len(), 24);
        for (i, (pk, writable)) in expected.iter().enumerate() {
            assert_eq!(
                ix.accounts[i].pubkey.to_string(),
                *pk,
                "account {i} mismatch"
            );
            assert_eq!(
                ix.accounts[i].is_writable, *writable,
                "account {i} writability mismatch"
            );
        }
        assert!(ix.accounts[1].is_signer, "user signs");
    }

    // ── buy layout (26 accounts) ──

    #[test]
    fn build_buy_ix_has_26_accounts_with_volume_and_buyback_tail() {
        // Live buy structure (sig 5Ls8BMWGf3JC…): IDL head + gva/uva +
        // fee_config/fee_program + cashback-era tail. We additionally
        // pass `pool_v2` ahead of the buyback pair (required by the
        // deployed handler — omitting it reverts with Overflow 6023;
        // verified by live simulation in tests/live_dex_sim.rs).
        let keys = live_keys();
        let ix = build_buy_ix(&LIVE_USER, &keys, 100, 1_000_000);
        assert_eq!(ix.program_id, PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 26);
        // Head 0..=18 identical to sells.
        let sell = build_sell_ix(&LIVE_USER, &keys, 1, 1);
        for i in 0..=18 {
            assert_eq!(ix.accounts[i].pubkey, sell.accounts[i].pubkey, "head {i}");
            assert_eq!(
                ix.accounts[i].is_writable, sell.accounts[i].is_writable,
                "head {i} writability"
            );
        }
        // Buy tail: gva · uva(W) · fee_config · fee_program · pool_v2 ·
        // buyback pair.
        assert_eq!(ix.accounts[19].pubkey, global_volume_accumulator_pda());
        assert!(!ix.accounts[19].is_writable);
        assert_eq!(
            ix.accounts[20].pubkey,
            user_volume_accumulator_pda(&LIVE_USER)
        );
        assert!(ix.accounts[20].is_writable);
        assert_eq!(ix.accounts[21].pubkey, fee_config_pda());
        assert_eq!(ix.accounts[22].pubkey, FEE_PROGRAM_ID);
        // pool_v2 — matches live sell tx account 21.
        assert_eq!(
            ix.accounts[23].pubkey,
            pubkey!("FGfL9sVbxWkPEErAgmHbVBoww4fW94CQGbf8RENj4EHK")
        );
        assert_eq!(ix.accounts[23].pubkey, pool_v2_pda(&LIVE_BASE));
        assert!(!ix.accounts[23].is_writable);
        assert_eq!(ix.accounts[24].pubkey, BUYBACK_FEE_RECIPIENT);
        assert!(!ix.accounts[24].is_writable);
        // Buyback recipient's WSOL ATA — matches live sell tx account 23.
        assert_eq!(
            ix.accounts[25].pubkey,
            pubkey!("HjQjngTDqoHE6aaGhUqfz9aQ7WZcBRjy5xB8PScLSr8i")
        );
        assert!(ix.accounts[25].is_writable);
    }

    #[test]
    fn build_sell_ix_uses_sell_discriminator() {
        let ix = build_sell_ix(&LIVE_USER, &live_keys(), 5_000, 12_345);
        assert_eq!(&ix.data[..8], &DISCRIM_SELL);
        assert_eq!(
            u64::from_le_bytes(ix.data[8..16].try_into().unwrap()),
            5_000
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[16..24].try_into().unwrap()),
            12_345
        );
        assert_eq!(ix.data.len(), 24);
    }

    // ── quote math ───────────────────────────────────────────────

    #[test]
    fn buy_quote_applies_protocol_fee_on_input() {
        let out = buy_quote(1_000_000_000, 1_000_000_000, 100_000_000_000).unwrap();
        assert!(out > 9_000_000 && out < 10_500_000, "got {out}");
    }

    #[test]
    fn buy_quote_zero_reserves_returns_none() {
        assert!(buy_quote(1, 0, 1_000_000).is_none());
        assert!(buy_quote(1, 1_000_000, 0).is_none());
    }

    #[test]
    fn sell_quote_applies_protocol_fee_on_output() {
        let out = sell_quote(10_000_000, 1_000_000_000, 100_000_000_000).unwrap();
        assert!(out > 900_000_000 && out < 1_000_000_000, "got {out}");
    }

    #[test]
    fn buy_then_sell_roundtrip_loses_to_double_fee() {
        let (base_r, quote_r) = (1_000_000_000_u64, 100_000_000_000_u64);
        let quote_in = 1_000_000_000_u64;
        let base_out = buy_quote(quote_in, base_r, quote_r).unwrap();
        let base_r2 = base_r - base_out;
        let quote_r2 = quote_r + (quote_in * (10_000 - PROTOCOL_FEE_BPS) / 10_000);
        let quote_back = sell_quote(base_out, base_r2, quote_r2).unwrap();
        let loss_bps = ((quote_in as i128 - quote_back as i128) * 10_000) / quote_in as i128;
        assert!(
            (180..=300).contains(&loss_bps),
            "expected 1.8-3% loss, got {loss_bps} bps"
        );
    }

    #[test]
    fn slippage_up_inflates_for_buy_max_in() {
        assert_eq!(apply_slippage(1_000_000, 100, true), 1_010_000);
    }

    #[test]
    fn slippage_down_deflates_for_min_out() {
        assert_eq!(apply_slippage(1_000_000, 100, false), 990_000);
    }

    // ── pool decoder ─────────────────────────────────────────────

    #[test]
    fn decode_live_pool_fixture() {
        let pool = live_pool();
        assert_eq!(pool.pool_bump, 250);
        assert_eq!(pool.index, 0);
        assert_eq!(pool.base_mint, LIVE_BASE);
        assert_eq!(pool.quote_mint, WSOL_MINT);
        // Vault addresses match live sell accounts 7 + 8.
        assert_eq!(
            pool.pool_base_token_account,
            pubkey!("6cczP83mihs9PNpfVAaPALaBP3FNh15FMrRvGprwVFja")
        );
        assert_eq!(
            pool.pool_quote_token_account,
            pubkey!("CcBYugEMNvPWkaM6deGkcdAJK7ipENwf3MQgykiyJ2tK")
        );
    }

    #[test]
    fn decode_pool_rejects_short_blob() {
        let r = decode_pool(&[0u8; 10]);
        assert!(matches!(r, Err(DecodeError::TooShort(10))));
    }

    #[test]
    fn decode_pool_rejects_bad_discriminator() {
        let mut data = base64::engine::general_purpose::STANDARD
            .decode(LIVE_POOL_B64)
            .unwrap();
        data[0] = 0xff;
        assert!(matches!(
            decode_pool(&data),
            Err(DecodeError::BadDiscriminator)
        ));
    }

    // ── full builders ────────────────────────────────────────────

    fn buy_params() -> BuyTxParams {
        let pool = live_pool();
        BuyTxParams {
            user: LIVE_USER,
            pool: LIVE_POOL,
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            base_token_program: ata::TOKEN_2022_PROGRAM_ID,
            quote_token_program: ata::TOKEN_PROGRAM_ID,
            coin_creator: pool.coin_creator,
            quote_in_amount: 100_000_000,
            slippage_bps: 100,
            reserves: PoolReserves {
                base: 1_000_000_000,
                quote: 100_000_000_000,
            },
            recent_blockhash: Hash::default(),
            compute_unit_limit: 250_000,
            compute_unit_price_micro_lamports: 50_000,
            tip_provider: TipProvider::None,
            tip_lamports: 0,
            tip_selector: TipSelector::new(0),
            skip_base_ata_create: false,
            skip_quote_ata_create: false,
        }
    }

    #[test]
    fn build_buy_tx_falcon_appends_tip_transfer() {
        let mut p = buy_params();
        p.tip_provider = TipProvider::Falcon;
        p.tip_lamports = 1_000_000;
        let bytes = build_buy_tx(&p).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 8 base ixs + tip = 9.
        match &tx.message {
            VersionedMessage::V0(m) => assert_eq!(m.instructions.len(), 9),
            _ => panic!("expected v0"),
        }
    }

    #[test]
    fn build_buy_tx_wraps_and_unwraps_wsol() {
        let bytes = build_buy_tx(&buy_params()).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 8 ixs: cu-limit + cu-price + ATA(base) + ATA(wsol) +
        // transfer + sync_native + buy + close(wsol).
        match &tx.message {
            VersionedMessage::V0(m) => assert_eq!(m.instructions.len(), 8),
            _ => panic!("expected v0"),
        }
    }

    #[test]
    fn build_buy_tx_skip_base_ata_drops_one_ix() {
        let mut p = buy_params();
        p.skip_base_ata_create = true;
        // skip_quote is IGNORED for WSOL quote — wrap always creates.
        p.skip_quote_ata_create = true;
        let bytes = build_buy_tx(&p).expect("build_buy_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 7 ixs: cu-limit + cu-price + ATA(wsol) + transfer + sync +
        // buy + close.
        match &tx.message {
            VersionedMessage::V0(m) => assert_eq!(m.instructions.len(), 7),
            _ => panic!("expected v0"),
        }
    }

    #[test]
    fn build_sell_tx_unwraps_wsol_proceeds() {
        let pool = live_pool();
        let p = SellTxParams {
            user: LIVE_USER,
            pool: LIVE_POOL,
            base_mint: pool.base_mint,
            quote_mint: pool.quote_mint,
            base_token_program: ata::TOKEN_2022_PROGRAM_ID,
            quote_token_program: ata::TOKEN_PROGRAM_ID,
            coin_creator: pool.coin_creator,
            base_in_amount: 1_000_000_000,
            slippage_bps: 100,
            reserves: PoolReserves {
                base: 1_000_000_000,
                quote: 100_000_000_000,
            },
            recent_blockhash: Hash::default(),
            compute_unit_limit: 200_000,
            compute_unit_price_micro_lamports: 50_000,
            tip_provider: TipProvider::None,
            tip_lamports: 0,
            tip_selector: TipSelector::new(0),
            skip_quote_ata_create: false,
        };
        let bytes = build_sell_tx(&p).expect("build_sell_tx");
        let tx: VersionedTransaction = bincode::deserialize(&bytes).unwrap();
        // 5 ixs: cu-limit + cu-price + ATA(wsol) + sell + close(wsol).
        match &tx.message {
            VersionedMessage::V0(m) => assert_eq!(m.instructions.len(), 5),
            _ => panic!("expected v0"),
        }
    }
}
