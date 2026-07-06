//! Swap-route dispatch — decides which DEX/aggregator builds the tx.
//!
//! For every buy signal the bot engine asks: is this mint trading on
//! a live PumpFun bonding curve, on the post-graduation PumpFun-AMM
//! pool, on a native Raydium AMM v4 pool, or does it need a generic
//! aggregator route? The answer drives whether we build the swap
//! natively or fall back to Jupiter.
//!
//! ## How the decision is made (`select_for_token_with_hint`)
//!
//! 0. If the caller supplies an `amm_hint` (a Raydium AMM v4 pool
//!    address from the signal payload):
//!    - Decode the AMM state; fetch market + vault balances concurrently.
//!    - On success → `Raydium` route.
//!    - On decode / RPC failure → fall through to PumpFun checks (the
//!      hint can be transiently stale; we don't hard-fail).
//! 1. Look up the deterministic `bonding_curve` PDA for the mint
//!    via `getAccountInfo`.
//!    - **Account decodes + `complete = false`** → live classic
//!      curve. Carry the decoded reserves through.
//!    - **Account decodes + `complete = true`** (graduated) → try
//!      step 2.
//!    - **Account does not exist** → try step 2.
//! 2. Look up the PumpSwap `pool` PDA at index=0 for `(mint, WSOL)`.
//!    - **Account decodes** → fetch base + quote vault balances,
//!      return `PumpFunAmm { pool_pubkey, pool, reserves }`.
//!    - **Account does not exist / decode fails / vault read
//!      fails** → fall through to Jupiter.
//! 3. **Default** → Jupiter (handles every chain + DEX combination).

use crate::dex::{
    ata,
    pumpfun::{self, BondingCurveAccount},
    pumpfun_amm::{self, PoolAccount, PoolReserves},
    raydium_amm_v4,
};
use crate::rpc::{RpcClient, RpcError};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("invalid mint pubkey: {0}")]
    InvalidMint(String),
    #[error("rpc: {0}")]
    Rpc(#[from] RpcError),
}

/// Routing decision for a single swap.
///
/// All native variants carry heap-allocated route payloads because the
/// inline state is ~100–400 bytes and would dominate the enum's stack
/// size. `Box` keeps every variant pointer-sized while preserving the
/// lossless-replay contract the dispatch path depends on.
#[derive(Debug, Clone)]
pub enum SwapRoute {
    /// Native PumpFun bonding-curve path. The decoded curve account
    /// is carried so the builder gets live virtual reserves without
    /// a second RPC; `token_program` is the mint's owner (legacy SPL
    /// Token or Token-2022 — pump launches both, audit M2).
    PumpFun(Box<PumpFunRoute>),
    /// Native PumpFun-AMM (Pumpswap) path — post-graduation venue.
    PumpFunAmm(Box<PumpFunAmmRoute>),
    /// Native Raydium AMM v4 path. Used when the signal carries an
    /// explicit AMM pool address (`amm_address`). The full decoded
    /// AMM + market state + vault reserves are pre-fetched by the
    /// route selector so the builder does zero RPC.
    Raydium(Box<RaydiumRoute>),
    /// Generic Jupiter route. Used for non-PumpFun mints AND for any
    /// case where the PumpFun-AMM resolution falls through (missing
    /// pool / vault read failure / decode mismatch).
    Jupiter,
}

/// Payload for `SwapRoute::PumpFun`. Lives heap-allocated.
#[derive(Debug, Clone)]
pub struct PumpFunRoute {
    pub curve: BondingCurveAccount,
    /// Owner program of the token mint — legacy SPL Token or
    /// Token-2022. Threads into every ATA derivation + the builders'
    /// `token_program` instruction account.
    pub token_program: Pubkey,
}

/// Payload for `SwapRoute::PumpFunAmm`. Lives heap-allocated.
#[derive(Debug, Clone)]
pub struct PumpFunAmmRoute {
    pub pool_pubkey: Pubkey,
    pub pool: PoolAccount,
    pub reserves: PoolReserves,
    /// Owner program of `pool.base_mint` (legacy or Token-2022).
    pub base_token_program: Pubkey,
    /// Owner program of `pool.quote_mint`. WSOL = legacy; pinned at
    /// route time for non-WSOL quote pools.
    pub quote_token_program: Pubkey,
}

/// Payload for `SwapRoute::Raydium`. Lives heap-allocated.
#[derive(Debug, Clone)]
pub struct RaydiumRoute {
    /// On-chain AMM account address (supplied externally as `amm_address`
    /// on the signal — Raydium pool addresses are NOT deterministic from
    /// the mint alone).
    pub amm_pubkey: Pubkey,
    /// Decoded Raydium AMM state (from `decode_amm_state`).
    pub amm: raydium_amm_v4::AmmState,
    /// Decoded OpenBook market state (from `decode_market_state`).
    pub market: raydium_amm_v4::MarketState,
    /// Live coin/pc vault balances at route-selection time.
    pub reserves: raydium_amm_v4::PoolReserves,
}

impl SwapRoute {
    /// Stable label for logging / metrics. Avoid `Debug` so we don't
    /// leak large decoded-state fields into log lines.
    pub fn label(&self) -> &'static str {
        match self {
            SwapRoute::PumpFun(_) => "pumpfun",
            SwapRoute::PumpFunAmm(_) => "pumpfun_amm",
            SwapRoute::Raydium(_) => "raydium_amm_v4",
            SwapRoute::Jupiter => "jupiter",
        }
    }
}

/// Look up the swap route for `token_mint`.
///
/// Thin wrapper around [`select_for_token_with_hint`] that passes
/// `amm_hint = None`. Kept for call sites that have no Raydium pool
/// hint (i.e. pure PumpFun/Jupiter flows).
pub async fn select_for_token(token_mint: &str, rpc: &RpcClient) -> Result<SwapRoute, RouteError> {
    select_for_token_with_hint(token_mint, None, rpc).await
}

/// Full route-selection logic, optionally accelerated by a Raydium
/// AMM pool address hint.
///
/// * `token_mint`  — base-58 token mint (non-quote side).
/// * `amm_hint`    — if `Some`, the Raydium AMM v4 account address
///   carried on the inbound signal (`signal.amm_address`). When
///   resolution succeeds the function returns `SwapRoute::Raydium`
///   immediately, bypassing the PumpFun checks. On any failure
///   (bad address, account missing, decode error, vault unavailable)
///   the function silently falls through to the PumpFun / Jupiter
///   path — the hint is advisory, not mandatory.
/// * `rpc`         — Solana RPC client (no network calls inside if
///   `amm_hint` resolves successfully in step 0).
pub async fn select_for_token_with_hint(
    token_mint: &str,
    amm_hint: Option<&str>,
    rpc: &RpcClient,
) -> Result<SwapRoute, RouteError> {
    // 0. Raydium fast-path — only attempted when the signal carries an
    //    explicit pool address. Raydium pool PDAs are NOT derivable
    //    from the mint alone; the caller must supply one.
    if let Some(amm_str) = amm_hint {
        if let Ok(amm_pk) = Pubkey::from_str(amm_str) {
            if let Ok(Some(amm_bytes)) = rpc.get_account_data(&amm_pk).await {
                if let Ok(amm) = raydium_amm_v4::decode_amm_state(&amm_bytes) {
                    // Fetch market data + both vault balances in parallel —
                    // three independent RPC calls over the same connection.
                    let (market_result, coin_result, pc_result) = tokio::join!(
                        rpc.get_account_data(&amm.market),
                        rpc.get_token_account_balance(&amm.token_coin),
                        rpc.get_token_account_balance(&amm.token_pc),
                    );
                    if let (Ok(Some(mkt_data)), Ok(Some(coin)), Ok(Some(pc))) =
                        (market_result, coin_result, pc_result)
                    {
                        if let Ok(market) = raydium_amm_v4::decode_market_state(&mkt_data) {
                            return Ok(SwapRoute::Raydium(Box::new(RaydiumRoute {
                                amm_pubkey: amm_pk,
                                amm,
                                market,
                                reserves: raydium_amm_v4::PoolReserves { coin, pc },
                            })));
                        }
                    }
                    // Market decode or vault read failed — fall through
                    // to PumpFun checks (transient RPC / stale data).
                }
            }
        }
    }

    let mint_pk = Pubkey::from_str(token_mint)
        .map_err(|_| RouteError::InvalidMint(token_mint.to_string()))?;

    // 1. Classic bonding-curve check — fastest path for sniping. The
    //    mint's owner program is fetched concurrently: pump launches
    //    Token-2022 mints since 2025 and the ATA derivations need the
    //    real owner (audit M2).
    let bc_pda = pumpfun::bonding_curve_pda(&mint_pk);
    let (bc_data, mint_owner) = tokio::join!(
        rpc.get_account_data(&bc_pda),
        rpc.get_account_owner(&mint_pk),
    );
    let bc_data = bc_data?;
    // Owner-fetch failures degrade to the legacy token program — the
    // pre-sign simulation catches a wrong guess and the engine falls
    // back to Jupiter, so a transient RPC blip never bricks the trade.
    let token_program = match mint_owner {
        Ok(Some(owner)) => owner,
        Ok(None) => ata::TOKEN_PROGRAM_ID,
        Err(e) => {
            tracing::warn!(mint = %mint_pk, error = %e,
                "route: mint owner fetch failed — assuming legacy token program");
            ata::TOKEN_PROGRAM_ID
        }
    };
    if let Some(bytes) = bc_data.as_deref() {
        if let Ok(bc) = pumpfun::decode_bonding_curve(bytes) {
            if !bc.complete {
                return Ok(SwapRoute::PumpFun(Box::new(PumpFunRoute {
                    curve: bc,
                    token_program,
                })));
            }
            // complete=true → fall through to AMM check
        }
        // discriminator mismatch on a curve PDA is theoretically
        // impossible (PDA owned by PumpFun program). Treat as
        // unknown → still try AMM.
    }

    // 2. PumpSwap pool. The pool PDA seeds include the pool CREATOR
    //    (["pool", index, creator, base_mint, quote_mint]); canonical
    //    graduated pools are created by the classic program's
    //    per-mint migration PDA, so the address IS derivable from the
    //    mint alone (audit M1 — the old creator-less derivation never
    //    matched any live pool). Permissionless pools with other
    //    creators fall to Jupiter.
    let pool_pubkey = pumpfun_amm::canonical_pool_pda(&mint_pk);
    if let Some(pool_bytes) = rpc.get_account_data(&pool_pubkey).await? {
        if let Ok(pool) = pumpfun_amm::decode_pool(&pool_bytes) {
            // Defensive: a pool whose mints don't match our expectation
            // means we resolved a stale / malformed account. Fall
            // through to Jupiter rather than building against the
            // wrong reserves.
            if pool.base_mint == mint_pk && pool.quote_mint == pumpfun_amm::WSOL_MINT {
                // Fetch live reserves concurrently — same RPC, two
                // requests over a connection pool.
                let (base_amt, quote_amt) = tokio::join!(
                    rpc.get_token_account_balance(&pool.pool_base_token_account),
                    rpc.get_token_account_balance(&pool.pool_quote_token_account),
                );
                if let (Ok(Some(base)), Ok(Some(quote))) = (base_amt, quote_amt) {
                    return Ok(SwapRoute::PumpFunAmm(Box::new(PumpFunAmmRoute {
                        pool_pubkey,
                        pool,
                        reserves: PoolReserves { base, quote },
                        base_token_program: token_program,
                        // Canonical pools are WSOL-quoted = legacy.
                        quote_token_program: ata::TOKEN_PROGRAM_ID,
                    })));
                }
                // Reserve read failed → don't gamble on stale
                // numbers; let Jupiter handle it.
            }
        }
    }

    // 3. Default: Jupiter.
    Ok(SwapRoute::Jupiter)
}

/// Backwards-compatible alias for `select_for_token` — preserves the
/// pre-slice-36 call site that exclusively used this for buys.
#[deprecated(note = "use select_for_token_with_hint; the function is symmetric for buys + sells")]
pub async fn select_for_buy(mint: &str, rpc: &RpcClient) -> Result<SwapRoute, RouteError> {
    select_for_token(mint, rpc).await
}

/// Pure decision function — extracted so unit tests can exercise every
/// branch without going through reqwest. `token_program` is the mint's
/// owner program (the network-fetched value in production; tests pass
/// the legacy id).
pub fn decide_from_account_data(data: Option<&[u8]>, token_program: Pubkey) -> SwapRoute {
    let Some(bytes) = data else {
        return SwapRoute::Jupiter;
    };
    match pumpfun::decode_bonding_curve(bytes) {
        Ok(bc) if !bc.complete => SwapRoute::PumpFun(Box::new(PumpFunRoute {
            curve: bc,
            token_program,
        })),
        // Graduated curve or bad discriminator both fall through to
        // Jupiter. Graduated is the common case; bad-discriminator
        // would imply a non-PumpFun program at this PDA (currently
        // impossible — PDA is owned by PumpFun program by derivation
        // — but we degrade safely if Solana's address space ever
        // changes).
        _ => SwapRoute::Jupiter,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::pumpfun::BONDING_CURVE_ACCOUNT_DISCRIM;

    fn fake_curve(complete: bool) -> Vec<u8> {
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
    fn missing_account_routes_to_jupiter() {
        let route = decide_from_account_data(None, ata::TOKEN_PROGRAM_ID);
        assert!(matches!(route, SwapRoute::Jupiter));
        assert_eq!(route.label(), "jupiter");
    }

    #[test]
    fn live_curve_routes_to_pumpfun() {
        let data = fake_curve(false);
        let route = decide_from_account_data(Some(&data), ata::TOKEN_2022_PROGRAM_ID);
        match route {
            SwapRoute::PumpFun(r) => {
                assert!(!r.curve.complete);
                assert_eq!(r.curve.virtual_sol_reserves, 30_000_000_000);
                assert_eq!(r.curve.virtual_token_reserves, 1_000_000_000_000_000);
                // The fetched mint owner travels with the route.
                assert_eq!(r.token_program, ata::TOKEN_2022_PROGRAM_ID);
            }
            _ => panic!("expected PumpFun route"),
        }
    }

    #[test]
    fn graduated_curve_falls_back_to_jupiter() {
        // `complete=true` means the curve has migrated; PumpFun buys
        // would revert. Jupiter routes us through PumpFun-AMM / Raydium.
        let data = fake_curve(true);
        let route = decide_from_account_data(Some(&data), ata::TOKEN_PROGRAM_ID);
        assert!(matches!(route, SwapRoute::Jupiter));
    }

    #[test]
    fn bad_discriminator_falls_back_to_jupiter() {
        let mut data = fake_curve(false);
        data[0] = 0xff;
        let route = decide_from_account_data(Some(&data), ata::TOKEN_PROGRAM_ID);
        assert!(matches!(route, SwapRoute::Jupiter));
    }

    #[test]
    fn short_blob_falls_back_to_jupiter() {
        let route = decide_from_account_data(Some(&[1u8, 2, 3]), ata::TOKEN_PROGRAM_ID);
        assert!(matches!(route, SwapRoute::Jupiter));
    }

    #[test]
    fn select_for_token_rejects_invalid_mint() {
        let rpc = RpcClient::new("http://127.0.0.1:65535");
        let r = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { select_for_token("not-a-pubkey", &rpc).await });
        assert!(matches!(r, Err(RouteError::InvalidMint(_))));
    }

    #[test]
    fn raydium_label() {
        use crate::dex::raydium_amm_v4::{AmmState, MarketState, PoolReserves};
        // Build a dummy Raydium route (all zeroed pubkeys — routing tests
        // only care about the variant / label, not the field values).
        let zero = solana_sdk::pubkey::Pubkey::default();
        let amm = AmmState {
            status: 6,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            token_coin: zero,
            token_pc: zero,
            coin_mint: zero,
            pc_mint: zero,
            open_orders: zero,
            market: zero,
            serum_dex: zero,
            target_orders: zero,
        };
        let market = MarketState {
            vault_signer_nonce: 0,
            coin_vault: zero,
            pc_vault: zero,
            event_queue: zero,
            bids: zero,
            asks: zero,
        };
        let route = SwapRoute::Raydium(Box::new(RaydiumRoute {
            amm_pubkey: zero,
            amm,
            market,
            reserves: PoolReserves {
                coin: 1_000,
                pc: 2_000,
            },
        }));
        assert_eq!(route.label(), "raydium_amm_v4");
    }

    #[test]
    fn select_for_token_with_hint_rejects_invalid_mint_when_hint_absent() {
        // When no hint is given, invalid mint errors propagate normally.
        let rpc = RpcClient::new("http://127.0.0.1:65535");
        let r = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { select_for_token_with_hint("not-a-pubkey", None, &rpc).await });
        assert!(matches!(r, Err(RouteError::InvalidMint(_))));
    }

    #[test]
    fn select_for_token_with_hint_bad_hint_pubkey_falls_through_to_mint_err() {
        // An unparseable hint string is silently ignored. The function
        // then hits the mint-parse step and returns InvalidMint.
        let rpc = RpcClient::new("http://127.0.0.1:65535");
        let r = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                select_for_token_with_hint("not-a-pubkey", Some("also-not-a-pubkey"), &rpc).await
            });
        assert!(matches!(r, Err(RouteError::InvalidMint(_))));
    }
}
