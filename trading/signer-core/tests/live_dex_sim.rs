//! Live-mainnet verification of the native DEX builders (READ-ONLY).
//!
//! Builds REAL buy transactions for a live PumpFun curve and a live
//! PumpSwap pool, then runs them through `simulateTransaction`
//! (`sigVerify=false`, `replaceRecentBlockhash=true`) against mainnet.
//! Nothing is ever signed or submitted.
//!
//! Ignored by default (network + live-market dependent). Run manually:
//!
//! ```sh
//! LIVE_PUMP_MINT=<active bonding-curve mint> \
//! LIVE_AMM_MINT=<graduated pumpswap mint> \
//! LIVE_USER=<funded wallet pubkey> \
//! cargo test --test live_dex_sim -- --ignored --nocapture
//! ```
//!
//! Defaults point at the 2026-06-11 audit fixtures; pump tokens
//! graduate/die quickly, so pass fresh mints when re-running later.

use degenbox_signer_core::dex::{ata, pumpfun, pumpfun_amm};
use degenbox_signer_core::route::{self, SwapRoute};
use degenbox_signer_core::rpc::RpcClient;
use degenbox_signer_core::simulator::Simulator;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
#[ignore = "live mainnet RPC — run manually with --ignored"]
fn live_sim_pumpfun_classic_buy() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let rpc_url = env_or("LIVE_RPC", "https://api.mainnet-beta.solana.com");
        let rpc = RpcClient::new(rpc_url.clone());
        let mint = env_or("LIVE_PUMP_MINT", "AAS4ggQ6KjqjNf8LkB27S2DZDQrGo126KfT4cesQpump");
        // A wallet that must hold enough SOL for the sim (never signs).
        let user = Pubkey::from_str(&env_or(
            "LIVE_USER",
            "EYrMvPMFM6iNmEab9ujvfQCgXfovbsmGFHTSLHRhiPQb",
        ))
        .unwrap();

        let route = route::select_for_token(&mint, &rpc).await.expect("route");
        let r = match route {
            SwapRoute::PumpFun(r) => r,
            other => {
                eprintln!(
                    "SKIP: {mint} no longer routes to a live curve ({}) — pass a fresh LIVE_PUMP_MINT",
                    other.label()
                );
                return;
            }
        };
        eprintln!("curve: {:?} token_program={}", r.curve, r.token_program);

        let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
        let params = pumpfun::BuyTxParams {
            user,
            mint: Pubkey::from_str(&mint).unwrap(),
            creator: r.curve.creator,
            token_program: r.token_program,
            sol_in_lamports: 5_000_000, // 0.005 SOL
            slippage_bps: 500,
            virtual_sol_reserves: r.curve.virtual_sol_reserves,
            virtual_token_reserves: r.curve.virtual_token_reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: 150_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_token_ata_create: false,
        };
        let unsigned = pumpfun::build_buy_tx(&params).expect("build");
        let sim = Simulator::new(rpc_url);
        let outcome = sim.simulate(&b64(&unsigned), false).await.expect("simulate");
        assert!(
            !outcome.would_fail,
            "LIVE pumpfun classic buy sim FAILED: {:?}",
            outcome.failure_reason
        );
        eprintln!("OK: live pumpfun classic buy simulates clean");
    });
}

#[test]
#[ignore = "live mainnet RPC — run manually with --ignored"]
fn live_sim_pumpfun_classic_sell() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let rpc_url = env_or("LIVE_RPC", "https://api.mainnet-beta.solana.com");
        let rpc = RpcClient::new(rpc_url.clone());
        let mint = env_or(
            "LIVE_PUMP_MINT",
            "AAS4ggQ6KjqjNf8LkB27S2DZDQrGo126KfT4cesQpump",
        );
        // A wallet that currently HOLDS the token (find one via
        // getTokenLargestAccounts). Never signs.
        let Ok(holder) = std::env::var("LIVE_PUMP_HOLDER") else {
            eprintln!("SKIP: set LIVE_PUMP_HOLDER to a wallet holding the mint");
            return;
        };
        let user = Pubkey::from_str(&holder).unwrap();

        let route = route::select_for_token(&mint, &rpc).await.expect("route");
        let r = match route {
            SwapRoute::PumpFun(r) => r,
            other => {
                eprintln!("SKIP: {mint} not on a live curve ({})", other.label());
                return;
            }
        };
        let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
        let params = pumpfun::SellTxParams {
            user,
            mint: Pubkey::from_str(&mint).unwrap(),
            creator: r.curve.creator,
            token_program: r.token_program,
            token_in_amount: env_or("LIVE_SELL_AMOUNT", "1000000").parse().unwrap(),
            slippage_bps: 500,
            virtual_sol_reserves: r.curve.virtual_sol_reserves,
            virtual_token_reserves: r.curve.virtual_token_reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: 150_000,
            compute_unit_price_micro_lamports: 50_000,
        };
        let unsigned = pumpfun::build_sell_tx(&params).expect("build");
        let sim = Simulator::new(rpc_url);
        let outcome = sim
            .simulate(&b64(&unsigned), false)
            .await
            .expect("simulate");
        assert!(
            !outcome.would_fail,
            "LIVE pumpfun classic sell sim FAILED: {:?}",
            outcome.failure_reason
        );
        eprintln!("OK: live pumpfun classic sell simulates clean");
    });
}

#[test]
#[ignore = "live mainnet RPC — run manually with --ignored"]
fn live_sim_pumpswap_sell() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let rpc_url = env_or("LIVE_RPC", "https://api.mainnet-beta.solana.com");
        let rpc = RpcClient::new(rpc_url.clone());
        let mint = env_or(
            "LIVE_AMM_MINT",
            "7BdfmAP5gUAwWAj78Pn3axyLBU7aa6HC8gpfL81Hpump",
        );
        let Ok(holder) = std::env::var("LIVE_AMM_HOLDER") else {
            eprintln!("SKIP: set LIVE_AMM_HOLDER to a wallet holding the mint");
            return;
        };
        let user = Pubkey::from_str(&holder).unwrap();

        let route = route::select_for_token(&mint, &rpc).await.expect("route");
        let r = match route {
            SwapRoute::PumpFunAmm(r) => r,
            other => {
                eprintln!("SKIP: {mint} has no canonical pool ({})", other.label());
                return;
            }
        };
        let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
        let params = pumpfun_amm::SellTxParams {
            user,
            pool: r.pool_pubkey,
            base_mint: r.pool.base_mint,
            quote_mint: r.pool.quote_mint,
            base_token_program: r.base_token_program,
            quote_token_program: ata::TOKEN_PROGRAM_ID,
            coin_creator: r.pool.coin_creator,
            base_in_amount: env_or("LIVE_SELL_AMOUNT", "1000000").parse().unwrap(),
            slippage_bps: 500,
            reserves: r.reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: 250_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_quote_ata_create: false,
        };
        let unsigned = pumpfun_amm::build_sell_tx(&params).expect("build");
        let sim = Simulator::new(rpc_url);
        let outcome = sim
            .simulate(&b64(&unsigned), false)
            .await
            .expect("simulate");
        assert!(
            !outcome.would_fail,
            "LIVE pumpswap sell sim FAILED: {:?}",
            outcome.failure_reason
        );
        eprintln!("OK: live pumpswap sell (swap → unwrap) simulates clean");
    });
}

#[test]
#[ignore = "live mainnet RPC — run manually with --ignored"]
fn live_sim_pumpswap_buy() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let rpc_url = env_or("LIVE_RPC", "https://api.mainnet-beta.solana.com");
        let rpc = RpcClient::new(rpc_url.clone());
        let mint = env_or("LIVE_AMM_MINT", "7BdfmAP5gUAwWAj78Pn3axyLBU7aa6HC8gpfL81Hpump");
        let user = Pubkey::from_str(&env_or(
            "LIVE_USER",
            "EYrMvPMFM6iNmEab9ujvfQCgXfovbsmGFHTSLHRhiPQb",
        ))
        .unwrap();

        let route = route::select_for_token(&mint, &rpc).await.expect("route");
        let r = match route {
            SwapRoute::PumpFunAmm(r) => r,
            other => {
                eprintln!(
                    "SKIP: {mint} does not route to a canonical PumpSwap pool ({}) — pass a fresh LIVE_AMM_MINT",
                    other.label()
                );
                return;
            }
        };
        eprintln!(
            "pool {} reserves base={} quote={} base_tp={} coin_creator={}",
            r.pool_pubkey, r.reserves.base, r.reserves.quote, r.base_token_program, r.pool.coin_creator
        );
        // Belt & braces: the canonical PDA round-trips.
        assert_eq!(
            pumpfun_amm::canonical_pool_pda(&Pubkey::from_str(&mint).unwrap()),
            r.pool_pubkey
        );

        let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
        let params = pumpfun_amm::BuyTxParams {
            user,
            pool: r.pool_pubkey,
            base_mint: r.pool.base_mint,
            quote_mint: r.pool.quote_mint,
            base_token_program: r.base_token_program,
            quote_token_program: ata::TOKEN_PROGRAM_ID,
            coin_creator: r.pool.coin_creator,
            quote_in_amount: 5_000_000, // 0.005 SOL
            slippage_bps: 500,
            reserves: r.reserves,
            recent_blockhash: blockhash,
            compute_unit_limit: 250_000,
            compute_unit_price_micro_lamports: 50_000,
            skip_base_ata_create: false,
            skip_quote_ata_create: false,
        };
        let unsigned = pumpfun_amm::build_buy_tx(&params).expect("build");
        if std::env::var("LIVE_DUMP_TX").is_ok() {
            eprintln!("TX_B64={}", b64(&unsigned));
        }
        let sim = Simulator::new(rpc_url);
        let outcome = sim.simulate(&b64(&unsigned), false).await.expect("simulate");
        assert!(
            !outcome.would_fail,
            "LIVE pumpswap buy sim FAILED: {:?}",
            outcome.failure_reason
        );
        eprintln!("OK: live pumpswap buy (wrap → swap → unwrap) simulates clean");
    });
}
