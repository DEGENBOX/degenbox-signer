//! Offline emergency kill-switch — `hl-signer-desktop panic`.
//!
//! Cancels EVERY resting order and closes EVERY open position for the
//! configured HL master account, signing locally and POSTing straight to
//! HL — no DegenBox server round-trip. This is the true self-custody
//! panic button: it works even if the gateway is down, the network to it
//! is severed, or the user's account is paused server-side.
//!
//! Mirrors the v1 Go client's emergency-flatten, and complements the
//! server-driven `POST /api/hl/emergency/flatten` (which queues the same
//! cancels/closes through the daemon when the server IS reachable).
//!
//! Order of operations: cancel resting orders FIRST (so a resting limit
//! can't re-open exposure mid-flatten), then close positions reduce-only.

use crate::audit::{AuditEntry, AuditLog};
use crate::config::{self, NetworkChoice};
use crate::keystore;
use crate::{branding, read_passphrase};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use platform_hl_exchange::{
    actions::{CancelAction, CancelSpec, Grouping, LimitSpec, OrderAction, OrderType, OrderWire},
    new_cloid, AgentSigner, ExchangeClient, Network, OrderStatusEntry,
};
use std::collections::HashMap;
use std::path::PathBuf;

pub async fn run_panic(
    keystore_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    password_stdin: bool,
) -> Result<()> {
    let ks_path = keystore_path
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;
    let cfg = config::load(&cfg_path).context("load config — run `setup` first")?;

    let account = cfg.account_address.clone().ok_or_else(|| {
        anyhow!(
            "no account_address configured — run `register --account=0x…` first. \
             The panic switch needs your HL master wallet to enumerate positions."
        )
    })?;
    let network = match cfg.network {
        NetworkChoice::Mainnet => Network::Mainnet,
        NetworkChoice::Testnet => Network::Testnet,
    };

    println!("{}", branding::wordmark());
    println!("{}", branding::heading("PANIC — flatten everything"));
    println!(
        "  {} account {}  ({})",
        branding::warn("!"),
        branding::accent_bold(&account),
        branding::muted(if network.is_mainnet() {
            "mainnet"
        } else {
            "testnet"
        })
    );

    let pass = read_passphrase(password_stdin)?;
    let (secret_hex, _agent) = keystore::decrypt(&ks_path, pass.as_bytes())?;
    let signer =
        AgentSigner::from_hex(&secret_hex, network).map_err(|e| anyhow!("agent signer: {e}"))?;
    let hl = ExchangeClient::new(network).map_err(|e| anyhow!("hl client: {e}"))?;
    let info =
        crate::hl_info::HttpInfoClient::new(network).map_err(|e| anyhow!("hl info client: {e}"))?;
    let audit = config::default_audit_path()
        .ok()
        .and_then(|p| AuditLog::open(&p).ok());

    // coin → asset_id (offline, no server to resolve it).
    let meta = info.meta().await.map_err(|e| anyhow!("meta fetch: {e}"))?;
    let asset_id: HashMap<String, u32> = meta
        .iter()
        .map(|m| (m.name.to_ascii_uppercase(), m.asset_id))
        .collect();

    // ── 1. Cancel all resting orders ──────────────────────────────────
    let orders = info
        .open_orders(&account)
        .await
        .map_err(|e| anyhow!("openOrders fetch: {e}"))?;
    if orders.is_empty() {
        println!("  {} no resting orders", branding::tick());
    } else {
        let mut cancels = Vec::new();
        let mut skipped = 0usize;
        for o in &orders {
            match asset_id.get(&o.coin.to_ascii_uppercase()) {
                Some(&a) => cancels.push(CancelSpec { a, o: o.oid as u64 }),
                None => {
                    skipped += 1;
                    tracing::warn!(coin = %o.coin, "panic: unknown asset for open order — skipping cancel");
                }
            }
        }
        if skipped > 0 {
            println!(
                "  {} {skipped} order(s) had an unknown asset and were skipped",
                branding::warn("!")
            );
        }
        if !cancels.is_empty() {
            let n = cancels.len();
            let action = CancelAction::new(cancels);
            match hl.cancel(&action, &signer).await {
                Ok(_) => {
                    println!("  {} cancelled {n} resting order(s)", branding::tick());
                    record(&audit, "panic_cancel", None, "cancelled", None, None);
                }
                Err(e) => {
                    println!("  {} cancel-all failed: {e}", branding::warn("✗"));
                    record(
                        &audit,
                        "panic_cancel",
                        None,
                        "failed",
                        None,
                        Some(e.to_string()),
                    );
                }
            }
        }
    }

    // ── 2. Close all positions (reduce-only market) ───────────────────
    let positions = info
        .all_positions(&account)
        .await
        .map_err(|e| anyhow!("positions fetch: {e}"))?;
    if positions.is_empty() {
        println!("  {} no open positions", branding::tick());
    } else {
        for p in &positions {
            let upper = p.coin.to_ascii_uppercase();
            let Some(&aid) = asset_id.get(&upper) else {
                println!(
                    "  {} {} — unknown asset, cannot close (close manually!)",
                    branding::warn("✗"),
                    p.coin
                );
                record(
                    &audit,
                    "panic_close",
                    Some(p.coin.clone()),
                    "failed",
                    None,
                    Some("unknown asset id".into()),
                );
                continue;
            };
            // Closing a long (szi>0) = sell; closing a short = buy.
            let is_buy = p.szi.is_sign_negative();
            let size = p.szi.abs().normalize().to_string();
            let cloid = new_cloid();
            // IoC reduce-only "market": HL needs a limit px, so we send a
            // guard far past the touch (buy→very high, sell→very low). The
            // fill lands at the book price; the px is only a worst-case rail.
            let guard_px = if is_buy { "9999999999" } else { "0.0001" };
            let wire = OrderWire {
                a: aid,
                b: is_buy,
                p: guard_px.to_string(),
                s: size,
                r: true,
                t: OrderType {
                    limit: Some(LimitSpec { tif: "Ioc".into() }),
                    trigger: None,
                },
                c: Some(cloid.clone()),
            };
            let action = OrderAction::new(vec![wire], Grouping::Na);
            match hl.place_order(&action, &signer).await {
                Ok(res) => match res.statuses.first() {
                    Some(OrderStatusEntry::Filled { oid, total_sz, .. }) => {
                        println!(
                            "  {} closed {} ({} @ market)",
                            branding::tick(),
                            p.coin,
                            total_sz
                        );
                        record(
                            &audit,
                            "panic_close",
                            Some(p.coin.clone()),
                            "filled",
                            Some(*oid as i64),
                            None,
                        );
                    }
                    Some(OrderStatusEntry::Resting { oid }) => {
                        println!(
                            "  {} {} close resting (oid {oid}) — re-run if it didn't fill",
                            branding::warn("~"),
                            p.coin
                        );
                        record(
                            &audit,
                            "panic_close",
                            Some(p.coin.clone()),
                            "submitted",
                            Some(*oid as i64),
                            None,
                        );
                    }
                    Some(OrderStatusEntry::Error(e)) => {
                        println!("  {} {} close rejected: {e}", branding::warn("✗"), p.coin);
                        record(
                            &audit,
                            "panic_close",
                            Some(p.coin.clone()),
                            "failed",
                            None,
                            Some(e.clone()),
                        );
                    }
                    Some(OrderStatusEntry::WaitingTrigger) => {
                        // A reduce-only market close should never rest as a
                        // trigger, but the variant exists on the enum — treat it
                        // as an accepted/armed submission (like Resting), not an
                        // error, so a panic-close is never mis-logged as failed.
                        println!(
                            "  {} {} close armed (waiting) — re-run if it didn't fill",
                            branding::warn("~"),
                            p.coin
                        );
                        record(
                            &audit,
                            "panic_close",
                            Some(p.coin.clone()),
                            "submitted",
                            None,
                            None,
                        );
                    }
                    None => {
                        println!(
                            "  {} {} close — no status returned",
                            branding::warn("✗"),
                            p.coin
                        );
                        record(
                            &audit,
                            "panic_close",
                            Some(p.coin.clone()),
                            "failed",
                            None,
                            Some("no status".into()),
                        );
                    }
                },
                Err(e) => {
                    println!(
                        "  {} {} close POST failed: {e}",
                        branding::warn("✗"),
                        p.coin
                    );
                    record(
                        &audit,
                        "panic_close",
                        Some(p.coin.clone()),
                        "failed",
                        None,
                        Some(e.to_string()),
                    );
                }
            }
        }
    }

    println!();
    println!(
        "  {} panic complete. Re-run to verify the book is flat.",
        branding::tick()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record(
    audit: &Option<AuditLog>,
    kind: &str,
    asset: Option<String>,
    status: &str,
    oid: Option<i64>,
    error: Option<String>,
) {
    if let Some(a) = audit {
        a.record_lossy(&AuditEntry {
            ts: Utc::now(),
            source: "panic",
            cloid: String::new(),
            kind: kind.to_string(),
            asset,
            status: status.to_string(),
            oid,
            error,
        });
    }
}
