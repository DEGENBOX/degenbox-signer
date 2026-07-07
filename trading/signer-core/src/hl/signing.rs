//! Translate a server-issued instruction payload into a signed HL
//! `/exchange` POST.
//!
//! Canonical merge of `hl-signer-desktop/src/signing.rs` (the live prod
//! executor — its semantics win) and the signer-app port. The payload
//! shape is produced by
//! `module_hyperliquid::exchange::executor::build_signer_payload` and
//! the various per-route enqueue paths (TP/SL on entry, updateLeverage,
//! vault transfer, closePosition, placeTpsl) — changing one side
//! without the other breaks the wire.

use crate::hl::info::{InfoClient, InfoError, LivePosition};
use platform_hl_exchange::{
    actions::{
        CancelAction, CancelByCloidAction, CancelByCloidSpec, CancelSpec, Grouping, LimitSpec,
        OrderAction, OrderType, OrderWire, TriggerWire, UpdateLeverageAction,
        UsdClassTransferAction, VaultTransferAction,
    },
    AgentSigner, ExchangeClient, ExchangeError, OrderStatusEntry,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SignError {
    #[error("payload decode: {0}")]
    Decode(String),
    #[error("hl exchange: {0}")]
    Exchange(#[from] ExchangeError),
    #[error("hl info: {0}")]
    Info(#[from] InfoError),
    #[error("missing account address — pair your HL master wallet first")]
    MissingAccount,
    #[error("bad payload: {0}")]
    BadPayload(String),
}

/// What we report back to the server's `/order/result` route.
///
/// `Serialize`/`Deserialize` so the daemon can persist a SUCCEEDED
/// result to its local executed-instruction marker store
/// (`exec_state`). That makes execution idempotent across `post_result`
/// retries: a re-polled row that already executed re-reports the cached
/// result instead of re-submitting to HL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedSubmitResult {
    pub cloid: String,
    pub oid: Option<i64>,
    pub status: String,
    pub filled_size_usd: Option<String>,
    /// Realised PnL on a closing/reducing fill. The gateway feeds this
    /// into the circuit-breaker loss counter.
    ///
    /// HL's `/exchange` `order` response (parsed into
    /// `OrderStatusEntry::Filled { oid, total_sz, avg_px }`) does NOT
    /// carry `closedPnl` — that only appears in `userFills`. So for a
    /// reduce-only close that FILLED we do a post-fill `/info userFills`
    /// lookup keyed by `oid` (see `resolve_closed_pnl`) and sum the
    /// matching rows' `closedPnl`. `None` means either a fresh OPEN (no
    /// realised PnL), a resting order (not filled yet), or the lookup
    /// couldn't correlate the oid in time — never a fabricated zero.
    pub closed_pnl: Option<String>,
    pub err_msg: Option<String>,
}

/// Per-instruction execution context. Bundles the agent signer, the
/// `/exchange` client, the `/info` client (for live-position lookups
/// needed by `closePosition` / `placeTpsl`) and the user's HL master
/// account address.
#[derive(Clone)]
pub struct ExecContext {
    pub signer: Arc<AgentSigner>,
    pub hl: ExchangeClient,
    pub info: Arc<dyn InfoClient>,
    /// User's HL master account (0x…). Required for `closePosition`
    /// and `placeTpsl`; unused by `order` / `cancel` /
    /// `updateLeverage` / `vaultTransfer`.
    pub account_address: Option<String>,
    /// M24: `"mainnet"` / `"testnet"` — this daemon's configured
    /// network. Every gateway instruction envelope carries a matching
    /// `network` pin; [`execute`] REFUSES a mismatched instruction so a
    /// testnet-configured signer can never sign gateway-mainnet asset
    /// ids (and vice versa). Envelopes without the field (pre-M24
    /// gateways) are accepted.
    pub network_tag: &'static str,
}

#[derive(Debug, Clone, Deserialize)]
struct OrderPayload {
    cloid: String,
    asset_id: u32,
    is_buy: bool,
    size: String,
    px: String,
    tif: String,
    /// B3: HL-native reduce-only flag. The gateway sends this on every
    /// order envelope (`build_signer_payload`'s `reduce_only`) — a copy
    /// mirror-close / FE reduce-only order MUST carry it onto the wire
    /// (`OrderWire.r`) or HL fills it as a fresh open and a stale
    /// notional can FLIP the position. `serde(default)` keeps old
    /// envelopes (no field) decoding as plain opens.
    #[serde(default)]
    reduce_only: bool,
    /// M12: per-order leverage requested by the gateway (copy-config
    /// `leverage_cap`, FE leverage field). When present on a
    /// NON-reduce-only order we issue an HL `updateLeverage` BEFORE the
    /// order so the margin mode matches the user's config. Deserialized
    /// as `i64` (the gateway sends an `i16`) and validated at use —
    /// never a decode failure for an out-of-range value.
    #[serde(default)]
    leverage: Option<i64>,
    /// Optional reduce-only TP trigger price (string-encoded). When
    /// either `tp_px` or `sl_px` is present we build a 3-leg bulk order
    /// with `Grouping::PositionTpsl`.
    #[serde(default)]
    tp_px: Option<String>,
    #[serde(default)]
    sl_px: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CancelPayload {
    cloid: String,
    asset_id: u32,
}

/// Payload for `kind: "cancelByOid"`. Cancels a resting order by its HL
/// exchange `oid` — preferred over cancel-by-cloid when the gateway knows
/// the oid (exact, no cloid-format ambiguity). `cloid` is carried only so
/// the signer reports the result back keyed on the gateway's ledger row.
#[derive(Debug, Clone, Deserialize)]
struct CancelByOidPayload {
    cloid: String,
    asset_id: u32,
    oid: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct LeveragePayload {
    cloid: String,
    asset_id: u32,
    leverage: u32,
    is_cross: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct VaultTransferPayload {
    cloid: String,
    vault_address: String,
    is_deposit: bool,
    /// USD in 6-decimal units. $100 → `100_000_000`.
    usd: u64,
}

/// Payload for `kind: "usdClassTransfer"` — a spot↔perp USDC move.
/// `usd` is 6-decimal units; the signer formats it to the human decimal
/// string HL's `usdClassTransfer` action wants. `to_perp = true` moves
/// spot→perp.
#[derive(Debug, Clone, Deserialize)]
struct UsdClassTransferPayload {
    cloid: String,
    to_perp: bool,
    /// USD in 6-decimal units. $100 → `100_000_000`.
    usd: u64,
}

/// Payload for `kind: "closePosition"`. The signer looks up the live
/// position on HL, computes `abs(szi) * percent / 100`, and submits a
/// reduce-only market order in the OPPOSITE direction.
#[derive(Debug, Clone, Deserialize)]
struct ClosePositionPayload {
    cloid: String,
    asset: String,
    asset_id: u32,
    /// String-encoded decimal in (0, 100].
    percent: String,
}

/// Payload for `kind: "placeTpsl"`. The signer queries the live
/// position to derive the exit side (long → sell, short → buy) and
/// sizes the trigger as `abs(szi) * close_percent / 100`, then POSTs
/// a single reduce-only trigger order with `Grouping::PositionTpsl`.
#[derive(Debug, Clone, Deserialize)]
struct PlaceTpslPayload {
    cloid: String,
    asset: String,
    asset_id: u32,
    /// `"tp"` or `"sl"` — passed straight through to HL.
    tp_sl: String,
    /// HL-precision-formatted trigger price (server already snapped to
    /// the asset's px_decimals).
    trigger_px: String,
    /// String-encoded decimal in (0, 100].
    close_percent: String,
}

/// Payload for `kind: "trailingSl"`. The server-side watcher manages
/// the trailing SL price independently; the signer only acks the row
/// so the gateway can clear it from `hl_pending_instructions`. Today
/// the server's `enqueue_trailing_sl` writes directly to
/// `hl_position_trailing_state` and does NOT enqueue a row here — this
/// handler exists as defensive parity for any future server path that
/// does queue an ack-only row.
#[derive(Debug, Clone, Deserialize)]
struct TrailingSlPayload {
    cloid: String,
}

/// M24: does the instruction's network pin match this daemon's
/// configured network? `None` (field absent — pre-M24 gateway) passes
/// for backward compat; a PRESENT-but-different tag is a hard refusal.
/// Pure so the rule is unit-testable.
fn network_pin_ok(payload_network: Option<&str>, ctx_tag: &str) -> bool {
    match payload_network {
        None => true,
        Some(tag) => tag.eq_ignore_ascii_case(ctx_tag),
    }
}

/// Inspect the payload and dispatch to the right HL action.
pub async fn execute(
    payload: &serde_json::Value,
    ctx: &ExecContext,
) -> Result<SignedSubmitResult, SignError> {
    // M24 network pin: refuse to sign an instruction stamped for a
    // DIFFERENT network — asset ids index a different universe there,
    // so signing it would trade the wrong asset. Reported back as a
    // failed result (the gateway's reclaim path never strands it).
    let wire_network = payload.get("network").and_then(|v| v.as_str());
    if !network_pin_ok(wire_network, ctx.network_tag) {
        return Err(SignError::BadPayload(format!(
            "network mismatch: instruction is for {}, this signer is configured for {} — refusing to sign",
            wire_network.unwrap_or("?"),
            ctx.network_tag
        )));
    }
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("order");
    match kind {
        "cancel" => execute_cancel(payload, &ctx.signer, &ctx.hl).await,
        "cancelByOid" => execute_cancel_by_oid(payload, &ctx.signer, &ctx.hl).await,
        "updateLeverage" => execute_update_leverage(payload, &ctx.signer, &ctx.hl).await,
        "vaultTransfer" => execute_vault_transfer(payload, &ctx.signer, &ctx.hl).await,
        "usdClassTransfer" => execute_usd_class_transfer(payload, &ctx.signer, &ctx.hl).await,
        "closePosition" => execute_close_position(payload, ctx).await,
        "placeTpsl" => execute_place_tpsl(payload, ctx).await,
        "trailingSl" => execute_trailing_sl(payload),
        _ => execute_order(payload, &ctx.signer, &ctx.hl).await,
    }
}

/// Build the HL `OrderAction` for an order payload. Pure so the wire
/// mapping — most critically `OrderWire.r` ← `payload.reduce_only` (B3)
/// — is unit-testable without an HL round-trip.
fn build_order_action(p: &OrderPayload) -> OrderAction {
    // Entry leg. B3: carry the gateway's reduce-only flag onto the wire —
    // hardcoding `r: false` here let copy mirror-closes FLIP positions.
    let entry = OrderWire {
        a: p.asset_id,
        b: p.is_buy,
        p: p.px.clone(),
        s: p.size.clone(),
        r: p.reduce_only,
        t: OrderType {
            limit: Some(LimitSpec { tif: p.tif.clone() }),
            trigger: None,
        },
        c: Some(p.cloid.clone()),
    };

    let has_tpsl = p.tp_px.is_some() || p.sl_px.is_some();
    let (orders, grouping) = if has_tpsl {
        // Bulk order: entry + reduce-only TP + reduce-only SL.
        // TP/SL legs flip the side (long entry → short exit) and use
        // a market trigger so they fire at the worst-case slip-tolerant
        // price.
        let mut legs = vec![entry];
        let exit_side = !p.is_buy;
        if let Some(tp_px) = &p.tp_px {
            legs.push(OrderWire {
                a: p.asset_id,
                b: exit_side,
                // For market triggers HL recommends a worst-case px;
                // we pass the trigger px so the bulk validates.
                p: tp_px.clone(),
                s: p.size.clone(),
                r: true,
                t: OrderType {
                    limit: None,
                    trigger: Some(TriggerWire {
                        is_market: true,
                        trigger_px: tp_px.clone(),
                        tp_sl: "tp".into(),
                    }),
                },
                c: None,
            });
        }
        if let Some(sl_px) = &p.sl_px {
            legs.push(OrderWire {
                a: p.asset_id,
                b: exit_side,
                p: sl_px.clone(),
                s: p.size.clone(),
                r: true,
                t: OrderType {
                    limit: None,
                    trigger: Some(TriggerWire {
                        is_market: true,
                        trigger_px: sl_px.clone(),
                        tp_sl: "sl".into(),
                    }),
                },
                c: None,
            });
        }
        (legs, Grouping::PositionTpsl)
    } else {
        (vec![entry], Grouping::Na)
    };
    OrderAction::new(orders, grouping)
}

/// M12: leverage to apply before a non-reduce-only order, if any. Pure
/// validation: reduce-only orders never re-lever (they only shrink an
/// existing position — changing margin mode mid-close is wrong), and an
/// out-of-range value (gateway bug / future widening) is dropped rather
/// than bounced to HL. HL's own per-coin max still applies server-side.
fn leverage_to_apply(p: &OrderPayload) -> Option<u32> {
    if p.reduce_only {
        return None;
    }
    p.leverage
        .filter(|l| (1..=100).contains(l))
        .map(|l| l as u32)
}

async fn execute_order(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: OrderPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;

    // M12: when the gateway forwarded a per-order leverage (copy-config
    // cap), set it on HL BEFORE placing the order so the position opens
    // at the configured leverage. Cross margin (HL's account default —
    // the gateway's explicit `updateLeverage` instruction path carries
    // its own `is_cross` for isolated setups). Best-effort: a leverage
    // reject must never block the entry itself — the order's notional is
    // fixed either way, leverage only changes margin allocation.
    if let Some(lev) = leverage_to_apply(&p) {
        let action = UpdateLeverageAction::new(p.asset_id, lev, true);
        if let Err(e) = client.update_leverage(&action, signer).await {
            tracing::warn!(
                cloid = %p.cloid, asset_id = p.asset_id, leverage = lev, error = %e,
                "pre-order updateLeverage failed — placing order at current account leverage"
            );
        }
    }

    let action = build_order_action(&p);
    match client.place_order(&action, signer).await {
        Ok(result) => {
            // We only report the *entry* leg back (the cloid is on it);
            // TP/SL legs' resting oids are not surfaced to the server's
            // order table yet. First status row corresponds to the
            // first wire entry, which we control.
            let (oid, mut status, mut err, filled) = match result.statuses.first() {
                Some(OrderStatusEntry::Resting { oid }) => {
                    (Some(*oid as i64), "submitted", None, None)
                }
                Some(OrderStatusEntry::Filled {
                    oid,
                    total_sz,
                    avg_px,
                }) => (
                    Some(*oid as i64),
                    "filled",
                    None,
                    fill_usd(total_sz, avg_px),
                ),
                Some(OrderStatusEntry::Error(e)) => (None, "failed", Some(e.clone()), None),
                None => (None, "failed", Some("no status returned".into()), None),
            };
            // B4: the TP/SL protective legs ride AFTER the entry in `statuses`.
            // If the entry placed but a protective leg was rejected, the user
            // holds a LIVE position with no stop — surface it loudly instead of
            // silently dropping the SL/TP error (audit B4: "unprotected position
            // the user believes is bounded").
            if status != "failed" {
                let leg_errs: Vec<String> = result
                    .statuses
                    .iter()
                    .skip(1)
                    .filter_map(|s| match s {
                        OrderStatusEntry::Error(e) => Some(e.clone()),
                        _ => None,
                    })
                    .collect();
                if !leg_errs.is_empty() {
                    status = "filled_unprotected";
                    err = Some(format!(
                        "entry {} but protective leg(s) rejected: {}",
                        if filled.is_some() { "filled" } else { "placed" },
                        leg_errs.join("; ")
                    ));
                }
            }
            Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid,
                status: status.into(),
                filled_size_usd: filled,
                err_msg: err,
                closed_pnl: None,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

async fn execute_cancel(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: CancelPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    // The resting order was placed on HL with the NORMALIZED cloid
    // (`to_hl_cloid` strips the `sl:5:`/`tp:5:`/`close:5:` ledger prefix down
    // to the bare 0x-cloid). HL's cancelByCloid matches on that same wire
    // cloid, so normalize before cancelling — a raw prefixed cloid would never
    // match and the cancel would be a silent no-op (leaving the stop resting).
    let action = CancelByCloidAction::new(vec![CancelByCloidSpec {
        asset: p.asset_id,
        cloid: to_hl_cloid(&p.cloid),
    }]);
    match client.cancel_by_cloid(&action, signer).await {
        // IDEMPOTENT: HL returns envelope-status "ok" even when the order was
        // already cancelled / filled / never placed (the per-status row
        // carries the "already canceled" note). We treat the POST succeeding
        // as cancelled-success so a redelivered cancel is a no-op-success, not
        // a fatal error. We REPORT the original (prefixed) cloid so the
        // gateway's ledger-row match is preserved.
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "cancelled".into(),
            filled_size_usd: None,
            err_msg: None,
            closed_pnl: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

/// Cancel a resting order by its HL `oid` (exact). Preferred over
/// cancel-by-cloid when the gateway has the oid. Idempotent: HL's POST
/// succeeds (envelope "ok") even for an already-cancelled / unknown oid, so a
/// redelivered cancel is a no-op-success. Reports the gateway's `cloid` so the
/// ledger row finalises cleanly.
async fn execute_cancel_by_oid(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: CancelByOidPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    let action = CancelAction::new(vec![CancelSpec {
        a: p.asset_id,
        o: p.oid,
    }]);
    match client.cancel(&action, signer).await {
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: Some(p.oid as i64),
            status: "cancelled".into(),
            filled_size_usd: None,
            err_msg: None,
            closed_pnl: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: Some(p.oid as i64),
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

async fn execute_update_leverage(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: LeveragePayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    let action = UpdateLeverageAction::new(p.asset_id, p.leverage, p.is_cross);
    match client.update_leverage(&action, signer).await {
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            // We re-use the order_result route for ack — `submitted` is
            // the cleanest match (HL accepted, no fill semantics here).
            status: "submitted".into(),
            filled_size_usd: None,
            err_msg: None,
            closed_pnl: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

async fn execute_vault_transfer(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: VaultTransferPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    let action = VaultTransferAction::new(&p.vault_address, p.is_deposit, p.usd);
    match client.vault_transfer(&action, signer).await {
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "submitted".into(),
            filled_size_usd: None,
            err_msg: None,
            closed_pnl: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

async fn execute_usd_class_transfer(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: UsdClassTransferPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    // Fail-closed on a malformed / zero amount: never sign a $0 (or
    // overflow) transfer. The gateway already clamps to the source
    // balance; this is the signer-side belt-and-braces.
    if p.usd == 0 {
        return Err(SignError::BadPayload(
            "usdClassTransfer amount is 0 — refusing to sign".into(),
        ));
    }
    let amount = format_usd_6dp(p.usd);
    let nonce = client.next_nonce();
    let action =
        UsdClassTransferAction::new(client.network().is_mainnet(), amount, p.to_perp, nonce);
    match client.usd_class_transfer(&action, signer).await {
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "submitted".into(),
            filled_size_usd: None,
            err_msg: None,
            closed_pnl: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

/// Format a 6-decimal-place USD integer as the human decimal string HL
/// expects on the `usdClassTransfer` wire (`12_500_000` → `"12.5"`).
/// Trailing zeros trimmed; whole numbers render without a fractional part
/// (`100_000_000` → `"100"`).
fn format_usd_6dp(usd_6dp: u64) -> String {
    let whole = usd_6dp / 1_000_000;
    let frac = usd_6dp % 1_000_000;
    if frac == 0 {
        return whole.to_string();
    }
    // 6-digit zero-padded fraction, trailing zeros stripped.
    let frac_str = format!("{frac:06}");
    let trimmed = frac_str.trim_end_matches('0');
    format!("{whole}.{trimmed}")
}

// ─────────────────────── closePosition handler ───────────────────────

async fn execute_close_position(
    payload: &serde_json::Value,
    ctx: &ExecContext,
) -> Result<SignedSubmitResult, SignError> {
    let p: ClosePositionPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    let percent = parse_percent(&p.percent)?;
    let account = ctx
        .account_address
        .as_deref()
        .ok_or(SignError::MissingAccount)?;

    let live = match ctx.info.position_for(account, &p.asset).await {
        Ok(pos) => pos,
        Err(InfoError::NoPosition(_)) => {
            // Nothing to close — server already moved on; report as
            // cancelled so the row is finalised cleanly (not surfaced
            // as a HL rejection in the UI).
            return Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid: None,
                status: "cancelled".into(),
                filled_size_usd: None,
                err_msg: Some(format!("no open position for {}", p.asset)),
                closed_pnl: None,
            });
        }
        Err(e) => return Err(SignError::Info(e)),
    };

    // One perp-meta lookup gives us the asset's precision, used for BOTH the
    // size rounding (B3) and the guard price (B2).
    let sz_decimals = asset_sz_decimals(ctx, &p.asset).await;
    let raw_close_size = compute_close_size(live.szi, percent);
    // Live mark — needed for BOTH the guard price (B2) AND the $10-minimum
    // notional resolve below. One lookup serves both.
    let mark = ctx.info.mid_price(&p.asset).await.ok().flatten();
    // MIN-NOTIONAL RESOLVE (money-safety: exits must NEVER be silently
    // dropped). HL rejects any order below $10 notional (`Order must have
    // minimum value of $10`). A 50% close of a ~$20 position sits right on
    // that boundary and rounding / a small adverse move puts it UNDER →
    // reject → the user can't exit from the dashboard. `resolve_close_size`
    // (pure, unit-tested) bumps a sub-$10 requested close UP so it clears
    // the floor, and — when bumping would strand a sub-$10 un-closeable
    // remainder — closes the FULL position instead. When we can't price the
    // position (no mark) it falls through with the requested size unchanged:
    // fail-OPEN, never block a legitimate close on a missing quote.
    let full_size = live.szi.abs();
    let close_size = resolve_close_size(raw_close_size, full_size, mark);
    // B3: HL rejects sizes carrying more decimals than sz_decimals. Truncate
    // DOWN to the allowed precision — never closes MORE than the open position.
    let close_size = match sz_decimals {
        Some(dp) => close_size.round_dp_with_strategy(dp, rust_decimal::RoundingStrategy::ToZero),
        None => close_size,
    };
    // Long (szi > 0) → exit = sell (is_buy=false). Short → buy.
    let exit_is_buy = live.szi.is_sign_negative();
    // B2: a VALID guard price from the live mark ±5% (fill-guaranteeing side),
    // rounded to HL's price rules. Fall back to the old sentinel ONLY if we
    // truly can't get a mark price — better to attempt the exit than never.
    let guard_px = match (mark, sz_decimals) {
        (Some(mid), Some(dp)) => guard_price(mid, exit_is_buy, dp),
        (Some(mid), None) => guard_price(mid, exit_is_buy, 2),
        _ if exit_is_buy => "9999999999".to_string(),
        _ => "0.0001".to_string(),
    };
    submit_reduce_only_market(
        ctx,
        &p.cloid,
        p.asset_id,
        exit_is_buy,
        &format_size(close_size),
        &guard_px,
    )
    .await
}

/// HL's absolute minimum order notional. HL rejects any order (open OR
/// reduce/close) whose USD value is below this with `Order must have
/// minimum value of $10`.
const HL_MIN_ORDER_USD: Decimal = Decimal::from_parts(10, 0, 0, false, 0);

/// Resolve the COIN size to actually close so a percent-close can never be
/// silently rejected by HL's $10-minimum-notional rule (money-safety: an
/// EXIT must always go through). Pure so the decision is unit-testable.
///
/// - `requested` = `abs(szi) * percent/100` (the size the user asked to close).
/// - `full` = `abs(szi)` (the whole position).
/// - `mark` = live mid price; `None` when we couldn't quote the coin.
///
/// Decision (when `mark` is known and > 0):
///   * requested notional ≥ $10 → close `requested` as-is.
///   * requested < $10 but the FULL position is also < $10 → the whole
///     position is dust; close it ALL (bumping to $10 would overshoot).
///   * requested < $10 and FULL ≥ $10 → bump the close UP to the $10 floor,
///     BUT if that would leave a sub-$10 un-closeable remainder
///     (`full_notional - 10 < 10`), close the FULL position instead — never
///     strand dust the user then can't exit.
///
/// When `mark` is `None`/≤0 we can't compute notional, so return `requested`
/// unchanged: fail-OPEN (attempt the exit) rather than block it on a missing
/// quote. HL may still reject a genuinely-sub-$10 order in that rare case,
/// but we never make the close WORSE than before.
fn resolve_close_size(requested: Decimal, full: Decimal, mark: Option<Decimal>) -> Decimal {
    let Some(px) = mark.filter(|m| *m > Decimal::ZERO) else {
        return requested;
    };
    let requested_usd = requested * px;
    if requested_usd >= HL_MIN_ORDER_USD {
        return requested;
    }
    let full_usd = full * px;
    // Whole position is itself below the floor → close it all (a partial
    // can never satisfy the minimum here, and $10 would overshoot the
    // position). Reduce-only caps the fill at the live size regardless.
    if full_usd < HL_MIN_ORDER_USD {
        return full;
    }
    // Bumping the close up to exactly $10 would leave `full - 10` behind.
    // If that remainder is itself below $10 it's un-closeable dust — close
    // the FULL position instead so the user can actually get flat.
    if full_usd - HL_MIN_ORDER_USD < HL_MIN_ORDER_USD {
        return full;
    }
    // Bump the close size up to the $10 floor (size = $10 / mark).
    HL_MIN_ORDER_USD / px
}

fn parse_percent(raw: &str) -> Result<Decimal, SignError> {
    let pct = Decimal::from_str(raw).map_err(|e| SignError::BadPayload(format!("percent: {e}")))?;
    if pct <= Decimal::ZERO || pct > Decimal::from(100) {
        return Err(SignError::BadPayload(format!(
            "percent must be in (0, 100], got {pct}"
        )));
    }
    Ok(pct)
}

/// `abs(szi) * percent / 100`. Always returns a non-negative value;
/// caller flips the side via `is_buy`.
fn compute_close_size(szi: Decimal, percent: Decimal) -> Decimal {
    let abs = szi.abs();
    (abs * percent) / Decimal::from(100)
}

/// HL accepts size strings; rust_decimal's `to_string()` produces a
/// plain decimal without scientific notation. We trim trailing zeros
/// after the decimal point so `"1.50000"` becomes `"1.5"` — keeps the
/// /exchange POST tidy and matches the server's `format_size`
/// behaviour for the same precision contract.
fn format_size(d: Decimal) -> String {
    // `normalize()` strips trailing zeros so `1.50000` → `1.5` and
    // `0.0001000` → `0.0001`. Plain integers stay as `1`, `0` etc.
    d.normalize().to_string()
}

/// B1: normalize an instruction cloid into a valid HL `c` (client-order-id)
/// field — `0x` + exactly 32 hex chars (16 bytes).
///
/// The gateway tags non-entry instruction cloids with a semantic prefix
/// (`close:5:0x…`, `tp:5:0x…`, `sl:5:0x…`) so it can route the pending
/// instruction. Those prefixed strings are NOT valid HL cloids — HL bounces
/// the whole order with a cloid-format error, so every "close X%" / standalone
/// TP-SL on a live position silently failed at the wire. Entry orders mint a
/// bare `new_cloid()` and were unaffected.
///
/// The bare cloid is embedded as the final `:`-delimited segment, so we extract
/// and reuse it (preserves uniqueness + is deterministic → a retried close maps
/// to the same wire cloid, letting HL's own cloid dedup block a double-fill on a
/// poll-cursor replay). If no valid cloid is embedded we derive one
/// deterministically (FNV-1a 128-bit over the full id) — still valid + stable.
///
/// The signer always REPORTS the original (prefixed) `p.cloid` back to the
/// gateway, so this wire-only rewrite leaves pending-instruction matching intact.
fn to_hl_cloid(instruction_cloid: &str) -> String {
    let candidate = instruction_cloid
        .rsplit(':')
        .next()
        .unwrap_or(instruction_cloid);
    if is_valid_hl_cloid(candidate) {
        return candidate.to_ascii_lowercase();
    }
    format!("0x{:032x}", fnv1a_128(instruction_cloid.as_bytes()))
}

/// HL cloid = `0x` + exactly 32 lowercase/uppercase hex chars (16 bytes).
fn is_valid_hl_cloid(s: &str) -> bool {
    s.len() == 34 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// FNV-1a 128-bit hash. Deterministic, no external dep — used only to derive a
/// valid wire cloid from an id that doesn't embed one. Offset basis + prime per
/// the FNV spec.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// USD notional of a fill = filled coin size × average fill price. Both come
/// straight off HL's `Filled` status entry. The server persists this as
/// `filled_size_usd`; reporting raw COIN size here (the old bug) made every
/// full fill look "partial" and corrupted the gateway's exposure / daily-loss
/// / P&L math (audit E1). Returns `None` only if either field won't parse.
fn fill_usd(total_sz: &str, avg_px: &str) -> Option<String> {
    let sz = Decimal::from_str(total_sz).ok()?;
    let px = Decimal::from_str(avg_px).ok()?;
    Some((sz * px).normalize().to_string())
}

/// Look up the asset's HL size precision (`sz_decimals`) from perp meta.
/// `None` on any miss — callers fall back to unrounded size / a default.
async fn asset_sz_decimals(ctx: &ExecContext, asset: &str) -> Option<u32> {
    ctx.info
        .perp_meta()
        .await
        .ok()?
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(asset))
        .map(|m| m.sz_decimals)
}

/// Round a float to `figs` significant figures (handles both ≥1 and <1
/// magnitudes). Used to satisfy HL's "≤5 significant figures" price rule;
/// f64 is fine here since the guard price is intentionally ~5% off-market.
fn round_to_sig_figs(x: f64, figs: u32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let d = figs as f64 - x.abs().log10().ceil();
    let factor = 10f64.powf(d);
    (x * factor).round() / factor
}

/// A VALID reduce-only guard price: the live mark biased 5% toward the
/// fill-guaranteeing side (buy above / sell below), rounded to HL's perp
/// price rules — ≤5 significant figures AND ≤ (6 − sz_decimals) decimals.
/// Replaces the hardcoded 9999999999 / 0.0001 sentinels HL rejects (B2).
fn guard_price(mid: Decimal, is_buy: bool, sz_decimals: u32) -> String {
    let mid_f = mid.to_f64().unwrap_or(0.0);
    let biased = if is_buy { mid_f * 1.05 } else { mid_f * 0.95 };
    let sf = round_to_sig_figs(biased, 5);
    let max_decimals = 6u32.saturating_sub(sz_decimals);
    Decimal::from_f64_retain(sf)
        .unwrap_or(mid)
        .round_dp(max_decimals)
        .normalize()
        .to_string()
}

async fn submit_reduce_only_market(
    ctx: &ExecContext,
    cloid: &str,
    asset_id: u32,
    is_buy: bool,
    size: &str,
    guard_px: &str,
) -> Result<SignedSubmitResult, SignError> {
    // An IoC "market" reduce-only still needs a limit price. The caller passes
    // a VALID guard price (mark ±5%, HL-precision-rounded — see `guard_price`);
    // the fill happens at the book price, the px is just a fill-guaranteeing
    // rail. (The old hardcoded 9999999999 / 0.0001 sentinels violated HL's
    // price-format rules and got the close rejected — audit B2.)
    let guard_px = guard_px.to_string();
    let wire = OrderWire {
        a: asset_id,
        b: is_buy,
        p: guard_px,
        s: size.to_string(),
        r: true,
        t: OrderType {
            limit: Some(LimitSpec { tif: "Ioc".into() }),
            trigger: None,
        },
        // B1: the prefixed instruction cloid ("close:5:0x…") is not a valid HL
        // wire cloid — normalize before it hits `/exchange`.
        c: Some(to_hl_cloid(cloid)),
    };
    let action = OrderAction::new(vec![wire], Grouping::Na);
    match ctx.hl.place_order(&action, &ctx.signer).await {
        Ok(result) => {
            let (oid, status, err, filled) = match result.statuses.first() {
                Some(OrderStatusEntry::Resting { oid }) => {
                    (Some(*oid as i64), "submitted", None, None)
                }
                Some(OrderStatusEntry::Filled {
                    oid,
                    total_sz,
                    avg_px,
                }) => (
                    Some(*oid as i64),
                    "filled",
                    None,
                    fill_usd(total_sz, avg_px),
                ),
                Some(OrderStatusEntry::Error(e)) => (None, "failed", Some(e.clone()), None),
                None => (None, "failed", Some("no status returned".into()), None),
            };
            // This is a reduce-only close — when it actually FILLED we
            // can report realised PnL so the gateway's circuit-breaker
            // counts the loss. HL's order response omits `closedPnl`, so
            // look it up from `userFills` by oid. On any miss (race,
            // unknown account) we send `None` — never a fabricated zero.
            let closed_pnl = resolve_closed_pnl(ctx, oid, status).await;
            Ok(SignedSubmitResult {
                cloid: cloid.to_string(),
                oid,
                status: status.into(),
                filled_size_usd: filled,
                err_msg: err,
                closed_pnl,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: cloid.to_string(),
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

/// Resolve realised PnL for a closing/reducing fill via `userFills`.
///
/// Returns `Some(pnl_string)` only when the order actually filled, we
/// know the account + oid, and the fill is indexed with a matching row.
/// Everything else → `None` (the wire contract treats absent PnL as
/// "not counted" rather than zero). A fresh OPEN never reaches this —
/// only reduce-only close paths call it.
async fn resolve_closed_pnl(ctx: &ExecContext, oid: Option<i64>, status: &str) -> Option<String> {
    if status != "filled" {
        return None; // resting / failed → nothing realised yet
    }
    let oid = oid?;
    if oid < 0 {
        return None;
    }
    let account = ctx.account_address.as_deref()?;
    match ctx.info.closed_pnl_for_oid(account, oid as u64).await {
        Ok(Some(pnl)) => Some(pnl.normalize().to_string()),
        Ok(None) => {
            tracing::warn!(
                oid,
                "closedPnl not found in userFills after retries — reporting None"
            );
            None
        }
        Err(e) => {
            tracing::warn!(oid, error = %e, "userFills lookup failed — reporting closed_pnl None");
            None
        }
    }
}

// ─────────────────────── placeTpsl handler ───────────────────────

async fn execute_place_tpsl(
    payload: &serde_json::Value,
    ctx: &ExecContext,
) -> Result<SignedSubmitResult, SignError> {
    let p: PlaceTpslPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    if !matches!(p.tp_sl.as_str(), "tp" | "sl") {
        return Err(SignError::BadPayload(format!(
            "tp_sl must be \"tp\" or \"sl\", got {}",
            p.tp_sl
        )));
    }
    let close_percent = parse_percent(&p.close_percent)?;
    let account = ctx
        .account_address
        .as_deref()
        .ok_or(SignError::MissingAccount)?;

    let live: LivePosition = match ctx.info.position_for(account, &p.asset).await {
        Ok(pos) => pos,
        Err(InfoError::NoPosition(_)) => {
            return Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid: None,
                status: "cancelled".into(),
                filled_size_usd: None,
                err_msg: Some(format!("no open position for {}", p.asset)),
                closed_pnl: None,
            });
        }
        Err(e) => return Err(SignError::Info(e)),
    };

    let size = format_size(compute_close_size(live.szi, close_percent));
    let exit_is_buy = live.szi.is_sign_negative();

    let wire = OrderWire {
        a: p.asset_id,
        b: exit_is_buy,
        // For market triggers HL recommends sending the trigger px as
        // the limit px too — it's only used as a guard against the
        // tick the trigger fires on.
        p: p.trigger_px.clone(),
        s: size,
        r: true,
        t: OrderType {
            limit: None,
            trigger: Some(TriggerWire {
                is_market: true,
                trigger_px: p.trigger_px.clone(),
                tp_sl: p.tp_sl.clone(),
            }),
        },
        // B1: "tp:5:0x…" / "sl:5:0x…" are not valid HL wire cloids — normalize.
        c: Some(to_hl_cloid(&p.cloid)),
    };
    let action = OrderAction::new(vec![wire], Grouping::PositionTpsl);
    match ctx.hl.place_order(&action, &ctx.signer).await {
        Ok(result) => {
            // A trigger order rests until it fires — `Resting` is the
            // expected good-path outcome, NOT `Filled`.
            let (oid, status, err) = match result.statuses.first() {
                Some(OrderStatusEntry::Resting { oid }) => (Some(*oid as i64), "submitted", None),
                Some(OrderStatusEntry::Filled { oid, .. }) => {
                    // Edge: trigger price already true at submission;
                    // HL fires immediately. Still a successful path.
                    (Some(*oid as i64), "filled", None)
                }
                Some(OrderStatusEntry::Error(e)) => (None, "failed", Some(e.clone())),
                None => (None, "failed", Some("no status returned".into())),
            };
            // TP/SL is reduce-only. The common path RESTS (no realised
            // PnL until it later fires — that close is reported by a
            // separate userFills-driven path / server reconciliation).
            // Only the immediate-fill edge realises PnL right now, so we
            // resolve closedPnl on "filled" only.
            let closed_pnl = resolve_closed_pnl(ctx, oid, status).await;
            Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid,
                status: status.into(),
                filled_size_usd: None,
                err_msg: err,
                closed_pnl,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
            closed_pnl: None,
        }),
    }
}

// ─────────────────────── trailingSl handler ───────────────────────

fn execute_trailing_sl(payload: &serde_json::Value) -> Result<SignedSubmitResult, SignError> {
    let p: TrailingSlPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;
    // The server-side `trailing_watcher` owns the SL price; the signer
    // has nothing to POST to HL for this row. Ack with `submitted` so
    // the server's `/order/result` route removes it from the pending
    // queue. If the watcher later wants the signer to place an
    // initial SL it will issue a separate `placeTpsl` row.
    Ok(SignedSubmitResult {
        cloid: p.cloid,
        oid: None,
        status: "submitted".into(),
        filled_size_usd: None,
        err_msg: None,
        closed_pnl: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hl::info::InfoError;
    use async_trait::async_trait;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn fill_usd_is_size_times_price_not_raw_coin_size() {
        // A full 0.5 BTC fill at $60k is $30,000 of notional — NOT "0.5".
        // Reporting raw coin size is what made full fills look "partial".
        assert_eq!(fill_usd("0.5", "60000").as_deref(), Some("30000"));
        assert_eq!(fill_usd("2", "1500.5").as_deref(), Some("3001"));
        // Unparseable inputs -> None, never a fabricated number.
        assert_eq!(fill_usd("x", "1"), None);
        assert_eq!(fill_usd("1", ""), None);
    }

    /// $10-minimum-notional resolve (money-safety: an EXIT must never be
    /// silently dropped). Covers each branch of `resolve_close_size`.
    #[test]
    fn resolve_close_size_never_strands_the_exit() {
        // Position: 0.001 BTC @ $60k = $60 full notional.
        let px = dec!(60000);
        let full = dec!(0.001); // $60
                                // 50% close = 0.0005 = $30 ≥ $10 → unchanged.
        assert_eq!(
            resolve_close_size(dec!(0.0005), full, Some(px)),
            dec!(0.0005)
        );

        // ~$20 position, 50% close ≈ $10 boundary. full = 0.00033 BTC = $19.8.
        let full = dec!(0.00033); // ~$19.8
                                  // 50% requested = 0.000165 = ~$9.9 < $10. full $19.8 ≥ $10, and
                                  // remainder after a $10 close = $9.8 < $10 → close the FULL position
                                  // (never leave sub-$10 dust the user can't exit).
        let out = resolve_close_size(dec!(0.000165), full, Some(px));
        assert_eq!(out, full, "sub-$10 remainder → close full, not dust");

        // Bigger position where a $10 bump leaves a healthy remainder:
        // 0.01 BTC = $600. 1% requested = 0.0001 = $6 < $10 → bump to $10
        // worth (= 10/60000), remainder $590 ≥ $10 so a partial is fine.
        let full = dec!(0.01);
        let out = resolve_close_size(dec!(0.0001), full, Some(px));
        assert_eq!(
            out,
            HL_MIN_ORDER_USD / px,
            "bump sub-$10 request to the floor"
        );
        assert!(out * px >= HL_MIN_ORDER_USD);
        assert!(out < full, "a bumped partial must stay below the full size");

        // Whole position is dust: 0.0001 BTC = $6 total. Any % close is
        // sub-$10 AND full is sub-$10 → close it all.
        let full = dec!(0.0001); // $6
        assert_eq!(resolve_close_size(dec!(0.00005), full, Some(px)), full);

        // No mark → fail-open: return the requested size unchanged (never
        // block the exit on a missing quote).
        assert_eq!(
            resolve_close_size(dec!(0.000165), dec!(0.00033), None),
            dec!(0.000165)
        );
        // Zero/negative mark is treated as "unpriced".
        assert_eq!(
            resolve_close_size(dec!(0.000165), dec!(0.00033), Some(dec!(0))),
            dec!(0.000165)
        );
    }

    #[test]
    fn guard_price_is_valid_biased_and_precise() {
        // BTC ~60k: buy guard ~5% above, sell ~5% below, both ≤5 sig figs.
        let buy = guard_price(dec!(60000), true, 5);
        let sell = guard_price(dec!(60000), false, 5);
        assert_eq!(buy, "63000");
        assert_eq!(sell, "57000");
        // Small-price coin keeps decimals within (6 - sz_decimals).
        let small = guard_price(dec!(0.5), true, 0);
        let v: f64 = small.parse().unwrap();
        assert!(v > 0.5 && v < 0.6, "small-coin buy guard: {small}");
        // Never emits the old invalid sentinels.
        assert_ne!(buy, "9999999999");
        assert_ne!(sell, "0.0001");
    }

    /// B1: prefixed instruction cloids must normalize to a valid HL wire cloid.
    #[test]
    fn to_hl_cloid_extracts_embedded_bare_cloid() {
        let bare = "0x0190f3a1b2c3d4e5f60718293a4b5c6d"; // 0x + 32 hex
        assert!(is_valid_hl_cloid(bare));
        // Each prefixed form embeds the bare cloid as its last `:` segment.
        for prefixed in [
            format!("close:5:{bare}"),
            format!("tp:5:{bare}"),
            format!("sl:12:{bare}"),
        ] {
            let wire = to_hl_cloid(&prefixed);
            assert_eq!(wire, bare, "should extract bare cloid from {prefixed}");
            assert!(is_valid_hl_cloid(&wire));
        }
        // A bare cloid passes through unchanged (entry path).
        assert_eq!(to_hl_cloid(bare), bare);
    }

    #[test]
    fn to_hl_cloid_derives_valid_when_no_embedded_cloid() {
        // No valid 0x-cloid anywhere → deterministic FNV-derived, still valid.
        let a = to_hl_cloid("close:5:not-a-cloid");
        let b = to_hl_cloid("close:5:not-a-cloid");
        assert!(is_valid_hl_cloid(&a), "derived cloid must be valid: {a}");
        assert_eq!(a, b, "derivation must be deterministic (retry idempotency)");
        // Distinct inputs → distinct cloids.
        assert_ne!(a, to_hl_cloid("close:5:different"));
    }

    /// Payload shape produced by the server's `build_signer_payload`
    /// must decode cleanly here. Pins the wire contract.
    #[test]
    fn order_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "cloid": "0xdeadbeef",
            "asset": "BTC",
            "asset_id": 0,
            "side": "buy",
            "is_buy": true,
            "order_kind": "market",
            "size_usd": "100",
            "size": "0.0015",
            "px": "65000",
            "tif": "Ioc",
            "leverage": null,
            "reduce_only": false,
            "intent_id": null,
            "reference_price": null
        });
        let p: OrderPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.cloid, "0xdeadbeef");
        assert_eq!(p.asset_id, 0);
        assert!(p.is_buy);
        assert_eq!(p.tif, "Ioc");
        assert!(p.tp_px.is_none());
        assert!(p.sl_px.is_none());
    }

    /// B3: a reduce-only envelope must decode the flag AND carry it onto
    /// the wire (`OrderWire.r`). The old payload silently DROPPED
    /// `reduce_only` (and hardcoded `r: false`), so a copy mirror-close
    /// could overshoot a stale notional and FLIP the position.
    #[test]
    fn reduce_only_decodes_and_reaches_the_wire() {
        let v = json!({
            "version": 1,
            "cloid": "0x0190f3a1b2c3d4e5f60718293a4b5c6d",
            "asset": "BTC",
            "asset_id": 0,
            "side": "sell",
            "is_buy": false,
            "order_kind": "market",
            "size_usd": "100",
            "size": "0.0015",
            "px": "60000",
            "tif": "Ioc",
            "leverage": null,
            "reduce_only": true,
            "intent_id": "copy-close-1",
            "reference_price": "60100",
        });
        let p: OrderPayload = serde_json::from_value(v).unwrap();
        assert!(p.reduce_only, "reduce_only must decode");
        let action = build_order_action(&p);
        assert_eq!(action.orders.len(), 1);
        assert!(
            action.orders[0].r,
            "entry leg must be reduce-only on the wire (B3)"
        );
        // Absent flag (legacy envelope) defaults to a plain open.
        let legacy: OrderPayload = serde_json::from_value(json!({
            "cloid": "0xa", "asset_id": 0, "is_buy": true,
            "size": "1", "px": "10", "tif": "Ioc",
        }))
        .unwrap();
        assert!(!legacy.reduce_only);
        assert!(!build_order_action(&legacy).orders[0].r);
    }

    /// B3 follow-on: TP/SL protective legs stay reduce-only regardless of
    /// the entry's flag, and the entry keeps its own flag in a bulk.
    #[test]
    fn tpsl_bulk_keeps_entry_flag_and_reduce_only_legs() {
        let p: OrderPayload = serde_json::from_value(json!({
            "cloid": "0xa", "asset_id": 0, "is_buy": true,
            "size": "1", "px": "10", "tif": "Ioc",
            "reduce_only": false,
            "tp_px": "12", "sl_px": "9",
        }))
        .unwrap();
        let action = build_order_action(&p);
        assert_eq!(action.orders.len(), 3);
        assert!(!action.orders[0].r, "open entry stays non-reduce-only");
        assert!(action.orders[1].r, "TP leg is always reduce-only");
        assert!(action.orders[2].r, "SL leg is always reduce-only");
        // Exit legs flip the side.
        assert!(!action.orders[1].b);
        assert!(!action.orders[2].b);
    }

    /// M12: per-order leverage decodes and only applies to opening
    /// orders within HL's sane range; reduce-only / out-of-range → None.
    #[test]
    fn leverage_applies_to_opens_only_and_validates_range() {
        let mk = |lev: serde_json::Value, ro: bool| -> OrderPayload {
            serde_json::from_value(json!({
                "cloid": "0xa", "asset_id": 5, "is_buy": true,
                "size": "1", "px": "10", "tif": "Ioc",
                "leverage": lev, "reduce_only": ro,
            }))
            .unwrap()
        };
        assert_eq!(leverage_to_apply(&mk(json!(10), false)), Some(10));
        assert_eq!(
            leverage_to_apply(&mk(json!(10), true)),
            None,
            "reduce-only never re-levers"
        );
        assert_eq!(leverage_to_apply(&mk(json!(null), false)), None);
        assert_eq!(leverage_to_apply(&mk(json!(0), false)), None);
        assert_eq!(leverage_to_apply(&mk(json!(-3), false)), None);
        assert_eq!(leverage_to_apply(&mk(json!(101), false)), None);
    }

    /// TP/SL fields are optional and decode when present.
    #[test]
    fn order_payload_decodes_with_tp_sl() {
        let v = json!({
            "cloid": "0xa",
            "asset_id": 0,
            "is_buy": true,
            "size": "0.001",
            "px": "60000",
            "tif": "Ioc",
            "tp_px": "66000",
            "sl_px": "54000",
        });
        let p: OrderPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.tp_px.as_deref(), Some("66000"));
        assert_eq!(p.sl_px.as_deref(), Some("54000"));
    }

    #[test]
    fn cancel_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "cancel",
            "cloid": "0xbeef",
            "asset_id": 5,
        });
        let p: CancelPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.cloid, "0xbeef");
        assert_eq!(p.asset_id, 5);
    }

    /// `cancelByOid` payload (cancel-replace + CancelTrade path) decodes the
    /// server shape produced by `cancel_resting_orders` when an oid is known.
    #[test]
    fn cancel_by_oid_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "cancelByOid",
            "cloid": "sl:5:0x0190f3a1b2c3d4e5f60718293a4b5c6d",
            "asset_id": 5,
            "oid": 123456789_u64,
        });
        let p: CancelByOidPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.asset_id, 5);
        assert_eq!(p.oid, 123_456_789);
        assert!(p.cloid.starts_with("sl:5:"));
    }

    /// The cancel-by-cloid path must NORMALIZE the prefixed ledger cloid down
    /// to the bare HL wire cloid before cancelling — otherwise HL never matches
    /// the resting order and the stop stays alive. Pins the mapping used in
    /// `execute_cancel`.
    #[test]
    fn cancel_normalizes_prefixed_cloid_to_wire_cloid() {
        let bare = "0x0190f3a1b2c3d4e5f60718293a4b5c6d";
        let prefixed = format!("sl:5:{bare}");
        assert_eq!(
            to_hl_cloid(&prefixed),
            bare,
            "cancel must target the bare wire cloid HL actually rests under"
        );
    }

    #[test]
    fn leverage_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "updateLeverage",
            "cloid": "lev:0:abc",
            "asset_id": 0,
            "leverage": 10,
            "is_cross": false,
        });
        let p: LeveragePayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.asset_id, 0);
        assert_eq!(p.leverage, 10);
        assert!(!p.is_cross);
    }

    #[test]
    fn vault_transfer_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "vaultTransfer",
            "cloid": "vt:abc",
            "vault_address": "0xdfc24b077bc1425ad1dea75bcb6f8158e10df303",
            "is_deposit": true,
            "usd": 100_000_000_u64,
        });
        let p: VaultTransferPayload = serde_json::from_value(v).unwrap();
        assert!(p.is_deposit);
        assert_eq!(p.usd, 100_000_000);
        assert!(p.vault_address.starts_with("0x"));
    }

    #[test]
    fn missing_required_field_is_decode_error() {
        let v = json!({"cloid": "x"}); // no asset_id, size, etc.
        let err: Result<OrderPayload, _> = serde_json::from_value(v);
        assert!(err.is_err());
    }

    #[test]
    fn usd_class_transfer_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "usdClassTransfer",
            "cloid": "ct:p:abc",
            "to_perp": true,
            "usd": 12_500_000_u64,
        });
        let p: UsdClassTransferPayload = serde_json::from_value(v).unwrap();
        assert!(p.to_perp);
        assert_eq!(p.usd, 12_500_000);
        assert_eq!(p.cloid, "ct:p:abc");
    }

    /// 6dp → human decimal string, matching HL's `usdClassTransfer` amount
    /// wire format: whole numbers have no fraction, trailing zeros trimmed.
    #[test]
    fn format_usd_6dp_matches_hl_amount_string() {
        assert_eq!(format_usd_6dp(100_000_000), "100");
        assert_eq!(format_usd_6dp(12_500_000), "12.5");
        assert_eq!(format_usd_6dp(1_000_000), "1");
        assert_eq!(format_usd_6dp(1_234_567), "1.234567");
        assert_eq!(format_usd_6dp(1), "0.000001");
        assert_eq!(format_usd_6dp(50_000), "0.05");
    }

    /// A zero-amount usdClassTransfer is refused BEFORE any HL POST —
    /// fail-closed on a malformed amount (mirrors the gateway's clamp).
    #[tokio::test]
    async fn usd_class_transfer_zero_amount_fails_closed() {
        let signer =
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap();
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let payload = json!({
            "version": 1,
            "kind": "usdClassTransfer",
            "cloid": "ct:p:zero",
            "to_perp": true,
            "usd": 0_u64,
        });
        let r = execute_usd_class_transfer(&payload, &signer, &hl).await;
        assert!(
            matches!(r, Err(SignError::BadPayload(_))),
            "zero amount must fail-closed, got {r:?}"
        );
    }

    // ─── closePosition payload tests ───

    /// Server-shape produced by `enqueue_close_position`.
    #[test]
    fn close_position_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "closePosition",
            "cloid": "close:0:abc",
            "asset": "BTC",
            "asset_id": 0,
            "percent": "50",
            "intent_id": null,
        });
        let p: ClosePositionPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.cloid, "close:0:abc");
        assert_eq!(p.asset, "BTC");
        assert_eq!(p.asset_id, 0);
        assert_eq!(p.percent, "50");
    }

    #[test]
    fn place_tpsl_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "placeTpsl",
            "cloid": "tp:0:abc",
            "asset": "BTC",
            "asset_id": 0,
            "tp_sl": "tp",
            "trigger_px": "70000",
            "close_percent": "100",
            "intent_id": null,
        });
        let p: PlaceTpslPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.tp_sl, "tp");
        assert_eq!(p.trigger_px, "70000");
        assert_eq!(p.close_percent, "100");
    }

    #[test]
    fn trailing_sl_payload_decodes_from_server_shape() {
        let v = json!({
            "version": 1,
            "kind": "trailingSl",
            "cloid": "trail:0:abc",
        });
        let p: TrailingSlPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.cloid, "trail:0:abc");
    }

    // ─── Pure-fn business-logic tests ───

    #[test]
    fn parse_percent_accepts_valid_range() {
        assert_eq!(parse_percent("50").unwrap(), dec!(50));
        assert_eq!(parse_percent("0.01").unwrap(), dec!(0.01));
        assert_eq!(parse_percent("100").unwrap(), dec!(100));
    }

    #[test]
    fn parse_percent_rejects_out_of_range() {
        assert!(matches!(parse_percent("0"), Err(SignError::BadPayload(_))));
        assert!(matches!(
            parse_percent("100.01"),
            Err(SignError::BadPayload(_))
        ));
        assert!(matches!(
            parse_percent("-10"),
            Err(SignError::BadPayload(_))
        ));
        assert!(matches!(
            parse_percent("not-a-number"),
            Err(SignError::BadPayload(_))
        ));
    }

    #[test]
    fn compute_close_size_handles_long_and_short() {
        // Long 1.5 BTC, close 50%
        assert_eq!(compute_close_size(dec!(1.5), dec!(50)), dec!(0.75));
        // Short -2 BTC, close 100%
        assert_eq!(compute_close_size(dec!(-2.0), dec!(100)), dec!(2.0));
        // Long 0.0001 BTC, close 25%
        assert_eq!(compute_close_size(dec!(0.0001), dec!(25)), dec!(0.000025));
    }

    #[test]
    fn format_size_strips_trailing_zeros() {
        assert_eq!(format_size(dec!(1.5)), "1.5");
        assert_eq!(format_size(dec!(1.50000)), "1.5");
        assert_eq!(format_size(dec!(0.000025)), "0.000025");
    }

    #[test]
    fn trailing_sl_handler_acks_without_calling_hl() {
        let out = execute_trailing_sl(&json!({
            "kind": "trailingSl",
            "cloid": "trail:0:1",
        }))
        .unwrap();
        assert_eq!(out.cloid, "trail:0:1");
        assert_eq!(out.status, "submitted");
        assert!(out.err_msg.is_none());
    }

    // ─── End-to-end handler tests with mocked InfoClient ───
    //
    // We don't have an `ExchangeClient` mock readily available (its
    // surface is concrete, not a trait). For tests that only need to
    // verify pre-HL decisions (exit-side selection, size math, missing
    // account, no-position short-circuit) we don't actually need to
    // round-trip to HL — the handler returns early or errors first.

    struct FakeInfo {
        result: Mutex<Option<Result<LivePosition, InfoError>>>,
        last_call: Mutex<Option<(String, String)>>,
        /// Canned `closed_pnl_for_oid` answer + a record of the oid asked.
        pnl: Mutex<Result<Option<Decimal>, ()>>,
        pnl_oid_asked: Mutex<Option<u64>>,
    }

    impl FakeInfo {
        fn returning(r: Result<LivePosition, InfoError>) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Some(r)),
                last_call: Mutex::new(None),
                pnl: Mutex::new(Ok(None)),
                pnl_oid_asked: Mutex::new(None),
            })
        }

        fn with_pnl(self: Arc<Self>, pnl: Result<Option<Decimal>, ()>) -> Arc<Self> {
            *self.pnl.lock().unwrap() = pnl;
            self
        }
    }

    #[async_trait]
    impl InfoClient for FakeInfo {
        async fn position_for(&self, account: &str, coin: &str) -> Result<LivePosition, InfoError> {
            *self.last_call.lock().unwrap() = Some((account.to_string(), coin.to_string()));
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("FakeInfo.position_for called twice")
        }

        async fn closed_pnl_for_oid(
            &self,
            _account: &str,
            oid: u64,
        ) -> Result<Option<Decimal>, InfoError> {
            *self.pnl_oid_asked.lock().unwrap() = Some(oid);
            match &*self.pnl.lock().unwrap() {
                Ok(v) => Ok(*v),
                Err(()) => Err(InfoError::Decode("fake userFills error".into())),
            }
        }
    }

    fn ctx_with(info: Arc<FakeInfo>, account: Option<&str>) -> ExecContext {
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        ExecContext {
            signer,
            hl,
            info,
            account_address: account.map(str::to_string),
            network_tag: "testnet",
        }
    }

    // ─── resolve_closed_pnl tests (the producer of the wire field) ───

    #[tokio::test]
    async fn resolve_closed_pnl_returns_pnl_on_filled_close() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("x".into())))
            .with_pnl(Ok(Some(dec!(-42.5))));
        let ctx = ctx_with(info.clone(), Some("0xabc"));
        let pnl = resolve_closed_pnl(&ctx, Some(123), "filled").await;
        assert_eq!(pnl.as_deref(), Some("-42.5"));
        assert_eq!(*info.pnl_oid_asked.lock().unwrap(), Some(123));
    }

    #[tokio::test]
    async fn resolve_closed_pnl_none_for_open_resting_order() {
        // A fresh open / a resting trigger is NOT "filled" → never even
        // queries userFills, always reports None.
        let info = FakeInfo::returning(Err(InfoError::NoPosition("x".into())))
            .with_pnl(Ok(Some(dec!(99))));
        let ctx = ctx_with(info.clone(), Some("0xabc"));
        let pnl = resolve_closed_pnl(&ctx, Some(123), "submitted").await;
        assert!(pnl.is_none());
        assert!(
            info.pnl_oid_asked.lock().unwrap().is_none(),
            "must not query userFills for a non-filled order"
        );
    }

    #[tokio::test]
    async fn resolve_closed_pnl_none_when_oid_not_indexed() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("x".into()))).with_pnl(Ok(None));
        let ctx = ctx_with(info, Some("0xabc"));
        let pnl = resolve_closed_pnl(&ctx, Some(7), "filled").await;
        assert!(pnl.is_none());
    }

    #[tokio::test]
    async fn resolve_closed_pnl_none_when_lookup_errors() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("x".into()))).with_pnl(Err(()));
        let ctx = ctx_with(info, Some("0xabc"));
        let pnl = resolve_closed_pnl(&ctx, Some(7), "filled").await;
        assert!(
            pnl.is_none(),
            "lookup error must degrade to None, not fabricate"
        );
    }

    #[tokio::test]
    async fn resolve_closed_pnl_none_without_account() {
        let info =
            FakeInfo::returning(Err(InfoError::NoPosition("x".into()))).with_pnl(Ok(Some(dec!(5))));
        let ctx = ctx_with(info, None);
        let pnl = resolve_closed_pnl(&ctx, Some(7), "filled").await;
        assert!(pnl.is_none());
    }

    #[tokio::test]
    async fn resolve_closed_pnl_reports_zero_open_fill() {
        // An IoC reduce that HL marks closedPnl=0 still reports "0" (the
        // breaker treats 0 as a non-loss; we don't drop the signal).
        let info = FakeInfo::returning(Err(InfoError::NoPosition("x".into())))
            .with_pnl(Ok(Some(Decimal::ZERO)));
        let ctx = ctx_with(info, Some("0xabc"));
        let pnl = resolve_closed_pnl(&ctx, Some(7), "filled").await;
        assert_eq!(pnl.as_deref(), Some("0"));
    }

    /// We can't easily construct an `ExchangeClient` in unit tests
    /// (its `place_order` will actually POST). But the parts of
    /// `execute_close_position` that exercise our logic — argument
    /// validation, account lookup, no-position short-circuit — all
    /// return BEFORE the HL POST. We test those edges directly.
    #[tokio::test]
    async fn close_position_short_circuits_on_no_open_position() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("BTC".into())));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info,
            account_address: Some("0xabc".into()),
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "closePosition",
            "cloid": "close:0:1",
            "asset": "BTC",
            "asset_id": 0,
            "percent": "50",
        });
        let out = execute(&payload, &ctx).await.unwrap();
        assert_eq!(out.status, "cancelled");
        assert_eq!(out.cloid, "close:0:1");
        assert!(out.err_msg.as_deref().unwrap().contains("no open position"));
    }

    #[tokio::test]
    async fn close_position_errors_without_account_address() {
        let info = FakeInfo::returning(Ok(LivePosition {
            coin: "BTC".into(),
            szi: dec!(1.0),
            unrealized_pnl: None,
            entry_px: None,
        }));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info,
            account_address: None,
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "closePosition",
            "cloid": "close:0:1",
            "asset": "BTC",
            "asset_id": 0,
            "percent": "50",
        });
        let err = execute(&payload, &ctx).await.unwrap_err();
        assert!(matches!(err, SignError::MissingAccount));
    }

    #[tokio::test]
    async fn close_position_rejects_invalid_percent() {
        let info = FakeInfo::returning(Ok(LivePosition {
            coin: "BTC".into(),
            szi: dec!(1.0),
            unrealized_pnl: None,
            entry_px: None,
        }));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info,
            account_address: Some("0xabc".into()),
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "closePosition",
            "cloid": "close:0:1",
            "asset": "BTC",
            "asset_id": 0,
            "percent": "150",
        });
        let err = execute(&payload, &ctx).await.unwrap_err();
        assert!(matches!(err, SignError::BadPayload(_)));
    }

    #[tokio::test]
    async fn place_tpsl_short_circuits_on_no_open_position() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("BTC".into())));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info,
            account_address: Some("0xabc".into()),
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "placeTpsl",
            "cloid": "tp:0:1",
            "asset": "BTC",
            "asset_id": 0,
            "tp_sl": "tp",
            "trigger_px": "70000",
            "close_percent": "100",
        });
        let out = execute(&payload, &ctx).await.unwrap();
        assert_eq!(out.status, "cancelled");
        assert!(out.err_msg.as_deref().unwrap().contains("no open position"));
    }

    #[tokio::test]
    async fn place_tpsl_rejects_invalid_tp_sl_value() {
        let info = FakeInfo::returning(Ok(LivePosition {
            coin: "BTC".into(),
            szi: dec!(1.0),
            unrealized_pnl: None,
            entry_px: None,
        }));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info,
            account_address: Some("0xabc".into()),
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "placeTpsl",
            "cloid": "tp:0:1",
            "asset": "BTC",
            "asset_id": 0,
            "tp_sl": "stop",
            "trigger_px": "70000",
            "close_percent": "100",
        });
        let err = execute(&payload, &ctx).await.unwrap_err();
        assert!(matches!(err, SignError::BadPayload(_)));
    }

    /// Trailing-SL dispatch goes through `execute` and never touches
    /// the network or info client — confirm.
    #[tokio::test]
    async fn trailing_sl_via_execute_acks_without_io() {
        let info = FakeInfo::returning(Err(InfoError::NoPosition("never-called".into())));
        let signer = Arc::new(
            AgentSigner::from_hex(&"11".repeat(32), platform_hl_exchange::Network::Testnet)
                .unwrap(),
        );
        let hl = ExchangeClient::new(platform_hl_exchange::Network::Testnet).unwrap();
        let ctx = ExecContext {
            signer,
            hl,
            info: info.clone(),
            account_address: None, // not needed for trailingSl
            network_tag: "testnet",
        };
        let payload = json!({
            "kind": "trailingSl",
            "cloid": "trail:0:1",
        });
        let out = execute(&payload, &ctx).await.unwrap();
        assert_eq!(out.status, "submitted");
        assert!(
            info.last_call.lock().unwrap().is_none(),
            "info must not be called"
        );
    }
}

/// Signer half of the gateway↔signer WIRE-CONTRACT tests.
///
/// Mirrors `crates/modules/hyperliquid/src/exchange/contract_tests.rs`
/// over the shared golden fixtures in `trading/contract-fixtures/hl/`:
///
///   * every `instruction_*.json` (what the gateway's payload builders
///     emit) must DECODE into this module's payload structs with no
///     load-bearing field dropped — pinned by explicit field asserts
///     (`reduce_only` and `leverage` were exactly the fields the old
///     decoder silently dropped: audit B3/M12);
///   * every `result_*.json` must be EXACTLY what `ResultReq`
///     serializes (the gateway's `OrderResultBody` decodes the same
///     files on its side).
///
/// The inventory test fails when a fixture is added/removed so a wire
/// change on one side is loudly surfaced on the other.
#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::hl::server::ResultReq;
    use chrono::{DateTime, Utc};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contract-fixtures/hl")
    }

    fn load(name: &str) -> serde_json::Value {
        let path = fixture_dir().join(name);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse fixture {name}: {e}"))
    }

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ─── gateway → signer: instruction envelopes decode ───

    /// M24: every instruction fixture carries the network pin, and the
    /// pin gate accepts a match / absent field but REFUSES a mismatch —
    /// a testnet-configured signer must never sign gateway-mainnet
    /// asset ids (the same id indexes a different universe there).
    #[test]
    fn fixture_network_pin_present_and_enforced() {
        for f in [
            "instruction_order_market.json",
            "instruction_order_reduce_only.json",
            "instruction_order_tpsl_leverage.json",
            "instruction_close_position.json",
            "instruction_cancel.json",
            "instruction_cancel_by_oid.json",
            "instruction_update_leverage.json",
            "instruction_vault_transfer.json",
            "instruction_usd_class_transfer.json",
            "instruction_place_tpsl.json",
        ] {
            let v = load(f);
            let net = v.get("network").and_then(|n| n.as_str());
            assert_eq!(net, Some("mainnet"), "{f} must carry the network pin");
            assert!(
                network_pin_ok(net, "mainnet"),
                "{f}: matching pin must pass"
            );
            assert!(
                !network_pin_ok(net, "testnet"),
                "{f}: a testnet signer must REFUSE this mainnet instruction"
            );
        }
        // Pre-M24 envelope (no field) stays signable — backward compat.
        assert!(network_pin_ok(None, "mainnet"));
        assert!(network_pin_ok(None, "testnet"));
    }

    #[test]
    fn fixture_order_market_decodes() {
        let p: OrderPayload = serde_json::from_value(load("instruction_order_market.json"))
            .expect("market order envelope must decode");
        assert_eq!(p.cloid, "0x0190f3a1b2c3d4e5f60718293a4b5c6d");
        assert_eq!(p.asset_id, 0);
        assert!(p.is_buy);
        assert_eq!(p.size, "0.0166");
        assert_eq!(p.px, "60300");
        assert_eq!(p.tif, "Ioc");
        assert!(!p.reduce_only);
        assert_eq!(p.leverage, None);
        assert!(p.tp_px.is_none() && p.sl_px.is_none());
    }

    /// B3: the field the old decoder silently DROPPED. If the gateway
    /// sends `reduce_only` it MUST survive decode and reach the wire.
    #[test]
    fn fixture_order_reduce_only_decodes_and_hits_the_wire() {
        let p: OrderPayload = serde_json::from_value(load("instruction_order_reduce_only.json"))
            .expect("reduce-only envelope must decode");
        assert!(p.reduce_only, "reduce_only must not be dropped (B3)");
        assert!(!p.is_buy);
        let action = build_order_action(&p);
        assert!(action.orders[0].r, "wire OrderWire.r must carry the flag");
        assert_eq!(
            leverage_to_apply(&p),
            None,
            "reduce-only never re-levers (M12)"
        );
    }

    /// M12 + H6: `leverage` decodes and applies on opens; the asset
    /// string carries HL's exact mixed casing end-to-end.
    #[test]
    fn fixture_order_tpsl_leverage_decodes() {
        let v = load("instruction_order_tpsl_leverage.json");
        assert_eq!(
            v.get("asset").and_then(|a| a.as_str()),
            Some("kPEPE"),
            "wire must carry HL exact casing (H6)"
        );
        let p: OrderPayload = serde_json::from_value(v).expect("tpsl+leverage envelope decode");
        assert_eq!(p.leverage, Some(10));
        assert_eq!(leverage_to_apply(&p), Some(10));
        assert_eq!(p.tp_px.as_deref(), Some("0.014"));
        assert_eq!(p.sl_px.as_deref(), Some("0.011"));
        let action = build_order_action(&p);
        assert_eq!(action.orders.len(), 3, "entry + TP + SL bulk");
        assert!(!action.orders[0].r);
        assert!(action.orders[1].r && action.orders[2].r);
    }

    #[test]
    fn fixture_close_position_decodes() {
        let p: ClosePositionPayload =
            serde_json::from_value(load("instruction_close_position.json"))
                .expect("closePosition envelope decode");
        assert_eq!(p.cloid, "close:0:50:exec-7");
        assert_eq!(p.asset, "BTC");
        assert_eq!(p.asset_id, 0);
        assert_eq!(parse_percent(&p.percent).unwrap(), Decimal::from(50));
    }

    #[test]
    fn fixture_cancel_decodes() {
        let p: CancelPayload = serde_json::from_value(load("instruction_cancel.json"))
            .expect("cancel envelope decode");
        assert_eq!(p.cloid, "0x0190f3a1b2c3d4e5f60718293a4b5c6d");
        assert_eq!(p.asset_id, 0);
    }

    #[test]
    fn fixture_cancel_by_oid_decodes() {
        let p: CancelByOidPayload = serde_json::from_value(load("instruction_cancel_by_oid.json"))
            .expect("cancelByOid envelope decode");
        assert_eq!(p.oid, 123_456_789);
        assert!(p.cloid.starts_with("sl:0:"));
    }

    #[test]
    fn fixture_update_leverage_decodes() {
        let p: LeveragePayload = serde_json::from_value(load("instruction_update_leverage.json"))
            .expect("updateLeverage envelope decode");
        assert_eq!(p.asset_id, 0);
        assert_eq!(p.leverage, 10);
        assert!(p.is_cross);
    }

    #[test]
    fn fixture_vault_transfer_decodes() {
        let p: VaultTransferPayload =
            serde_json::from_value(load("instruction_vault_transfer.json"))
                .expect("vaultTransfer envelope decode");
        assert!(p.is_deposit);
        assert_eq!(p.usd, 100_000_000);
        assert_eq!(
            p.vault_address,
            "0xdfc24b077bc1425ad1dea75bcb6f8158e10df303"
        );
    }

    #[test]
    fn fixture_usd_class_transfer_decodes() {
        let p: UsdClassTransferPayload =
            serde_json::from_value(load("instruction_usd_class_transfer.json"))
                .expect("usdClassTransfer envelope decode");
        assert!(p.to_perp, "spot→perp direction must survive decode");
        assert_eq!(p.usd, 12_500_000);
        assert_eq!(p.cloid, "ct:p:0x0190f3a1b2c3d4e5f60718293a4b5c74");
        // And it formats to the human amount HL wants on the wire.
        assert_eq!(format_usd_6dp(p.usd), "12.5");
    }

    #[test]
    fn fixture_place_tpsl_decodes() {
        let v = load("instruction_place_tpsl.json");
        assert_eq!(
            v.get("asset").and_then(|a| a.as_str()),
            Some("kPEPE"),
            "wire must carry HL exact casing (H6)"
        );
        let p: PlaceTpslPayload = serde_json::from_value(v).expect("placeTpsl envelope decode");
        assert_eq!(p.tp_sl, "sl");
        assert_eq!(p.trigger_px, "0.011");
        assert_eq!(p.close_percent, "100");
    }

    // ─── signer → gateway: result bodies serialize ───

    fn base_result(cloid: &str, status: &str) -> ResultReq {
        ResultReq {
            cloid: cloid.into(),
            oid: None,
            status: status.into(),
            filled_size_usd: None,
            closed_pnl: None,
            err_msg: None,
            signed_at: None,
            posted_to_hl_at: None,
        }
    }

    #[test]
    fn result_submitted_serializes_to_fixture() {
        let req = ResultReq {
            oid: Some(987_654_321),
            ..base_result("0x0190f3a1b2c3d4e5f60718293a4b5c6d", "submitted")
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_submitted.json")
        );
    }

    #[test]
    fn result_filled_serializes_to_fixture() {
        let req = ResultReq {
            oid: Some(987_654_322),
            filled_size_usd: Some("500.25".into()),
            closed_pnl: Some("-42.5".into()),
            signed_at: Some(ts("2026-06-11T12:00:00Z")),
            posted_to_hl_at: Some(ts("2026-06-11T12:00:01Z")),
            ..base_result("close:0:50:exec-7", "filled")
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_filled.json")
        );
    }

    #[test]
    fn result_failed_serializes_to_fixture() {
        let req = ResultReq {
            err_msg: Some("Order has invalid size".into()),
            ..base_result("0x0190f3a1b2c3d4e5f60718293a4b5c6d", "failed")
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_failed.json")
        );
    }

    #[test]
    fn result_cancelled_serializes_to_fixture() {
        let req = base_result("sl:0:0x0190f3a1b2c3d4e5f60718293a4b5c6d", "cancelled");
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_cancelled.json")
        );
    }

    /// B5: the dry-run status the gateway used to 400 on.
    #[test]
    fn result_paper_serializes_to_fixture() {
        let req = ResultReq {
            err_msg: Some("paper mode — dry run, not submitted to HL".into()),
            ..base_result("0x0190f3a1b2c3d4e5f60718293a4b5c6d", "paper")
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_paper.json")
        );
    }

    /// B5: filled-but-no-stop — the case that most needs to land.
    #[test]
    fn result_filled_unprotected_serializes_to_fixture() {
        let req = ResultReq {
            oid: Some(987_654_323),
            filled_size_usd: Some("1000".into()),
            err_msg: Some(
                "entry filled but protective leg(s) rejected: Order price must be within 80% of oracle"
                    .into(),
            ),
            ..base_result("0x0190f3a1b2c3d4e5f60718293a4b5c6d", "filled_unprotected")
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            load("result_filled_unprotected.json")
        );
    }

    // ─── drift alarm: fixture inventory ───

    #[test]
    fn fixture_inventory_is_fully_covered() {
        let expected: BTreeSet<&str> = [
            "README.md",
            "instruction_order_market.json",
            "instruction_order_reduce_only.json",
            "instruction_order_tpsl_leverage.json",
            "instruction_close_position.json",
            "instruction_cancel.json",
            "instruction_cancel_by_oid.json",
            "instruction_update_leverage.json",
            "instruction_vault_transfer.json",
            "instruction_usd_class_transfer.json",
            "instruction_place_tpsl.json",
            "result_submitted.json",
            "result_filled.json",
            "result_failed.json",
            "result_cancelled.json",
            "result_paper.json",
            "result_filled_unprotected.json",
        ]
        .into_iter()
        .collect();
        let actual: BTreeSet<String> = std::fs::read_dir(fixture_dir())
            .expect("fixture dir must exist")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        let actual_refs: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
        assert_eq!(
            actual_refs, expected,
            "fixture set drifted — cover the new/removed fixture in BOTH suites"
        );
    }
}
