//! Translate a server-issued instruction payload into a signed HL
//! `/exchange` POST.
//!
//! The payload shape is produced by
//! `module_hyperliquid::exchange::executor::build_signer_payload` and
//! the various per-route enqueue paths (TP/SL on entry, updateLeverage,
//! vault transfer, closePosition, placeTpsl) — changing one side
//! without the other breaks the wire.

use crate::hl_info::{InfoClient, InfoError, LivePosition};
use platform_hl_exchange::{
    actions::{
        CancelByCloidAction, CancelByCloidSpec, Grouping, LimitSpec, OrderAction, OrderType,
        OrderWire, TriggerWire, UpdateLeverageAction, VaultTransferAction,
    },
    AgentSigner, ExchangeClient, ExchangeError, OrderStatusEntry,
};
use rust_decimal::Decimal;
use serde::Deserialize;
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
    #[error("missing account address — set it via `hl-signer-desktop register --account=0x…`")]
    MissingAccount,
    #[error("bad payload: {0}")]
    BadPayload(String),
}

/// What we report back to the server's `/order/result` route.
#[derive(Debug, Clone)]
pub struct SignedSubmitResult {
    pub cloid: String,
    pub oid: Option<i64>,
    pub status: String,
    pub filled_size_usd: Option<String>,
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
}

#[derive(Debug, Clone, Deserialize)]
struct OrderPayload {
    cloid: String,
    asset_id: u32,
    is_buy: bool,
    size: String,
    px: String,
    tif: String,
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

/// Inspect the payload and dispatch to the right HL action.
pub async fn execute(
    payload: &serde_json::Value,
    ctx: &ExecContext,
) -> Result<SignedSubmitResult, SignError> {
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("order");
    match kind {
        "cancel" => execute_cancel(payload, &ctx.signer, &ctx.hl).await,
        "updateLeverage" => execute_update_leverage(payload, &ctx.signer, &ctx.hl).await,
        "vaultTransfer" => execute_vault_transfer(payload, &ctx.signer, &ctx.hl).await,
        "closePosition" => execute_close_position(payload, ctx).await,
        "placeTpsl" => execute_place_tpsl(payload, ctx).await,
        "trailingSl" => execute_trailing_sl(payload),
        _ => execute_order(payload, &ctx.signer, &ctx.hl).await,
    }
}

async fn execute_order(
    payload: &serde_json::Value,
    signer: &AgentSigner,
    client: &ExchangeClient,
) -> Result<SignedSubmitResult, SignError> {
    let p: OrderPayload =
        serde_json::from_value(payload.clone()).map_err(|e| SignError::Decode(e.to_string()))?;

    // Entry leg.
    let entry = OrderWire {
        a: p.asset_id,
        b: p.is_buy,
        p: p.px.clone(),
        s: p.size.clone(),
        r: false,
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

    let action = OrderAction::new(orders, grouping);
    match client.place_order(&action, signer).await {
        Ok(result) => {
            // We only report the *entry* leg back (the cloid is on it);
            // TP/SL legs' resting oids are not surfaced to the server's
            // order table yet. First status row corresponds to the
            // first wire entry, which we control.
            let (oid, status, err, filled) = match result.statuses.first() {
                Some(OrderStatusEntry::Resting { oid }) => {
                    (Some(*oid as i64), "submitted", None, None)
                }
                Some(OrderStatusEntry::Filled { oid, total_sz, .. }) => {
                    (Some(*oid as i64), "filled", None, Some(total_sz.clone()))
                }
                Some(OrderStatusEntry::Error(e)) => (None, "failed", Some(e.clone()), None),
                None => (None, "failed", Some("no status returned".into()), None),
            };
            Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid,
                status: status.into(),
                filled_size_usd: filled,
                err_msg: err,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
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
    let action = CancelByCloidAction::new(vec![CancelByCloidSpec {
        asset: p.asset_id,
        cloid: p.cloid.clone(),
    }]);
    match client.cancel_by_cloid(&action, signer).await {
        Ok(_) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "cancelled".into(),
            filled_size_usd: None,
            err_msg: None,
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
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
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
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
        }),
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
        }),
    }
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
            });
        }
        Err(e) => return Err(SignError::Info(e)),
    };

    let close_size = compute_close_size(live.szi, percent);
    // Long (szi > 0) → exit = sell (is_buy=false). Short → buy.
    let exit_is_buy = live.szi.is_sign_negative();
    submit_reduce_only_market(
        ctx,
        &p.cloid,
        p.asset_id,
        exit_is_buy,
        &format_size(close_size),
    )
    .await
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

async fn submit_reduce_only_market(
    ctx: &ExecContext,
    cloid: &str,
    asset_id: u32,
    is_buy: bool,
    size: &str,
) -> Result<SignedSubmitResult, SignError> {
    // For an IoC "market" order HL still requires a limit price. We
    // pass `"0"` (worst-case buy) / a huge number (worst-case sell)
    // — but HL actually rejects 0-priced orders, so use the agent's
    // own price-discovery via a wide guard rail: a sane convention
    // matching the server is to send the current oracle price ±5%.
    // Since the signer doesn't have a fresh quote here, we use a
    // sentinel: HL accepts ANY price for an IoC reduce-only fill as
    // long as it can be matched. For safety we use a px that errs on
    // the worst-case side: buy → very high, sell → very low. The fill
    // happens at the book price; the px is just a guard.
    let guard_px = if is_buy {
        "9999999999".to_string()
    } else {
        "0.0001".to_string()
    };
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
        c: Some(cloid.to_string()),
    };
    let action = OrderAction::new(vec![wire], Grouping::Na);
    match ctx.hl.place_order(&action, &ctx.signer).await {
        Ok(result) => {
            let (oid, status, err, filled) = match result.statuses.first() {
                Some(OrderStatusEntry::Resting { oid }) => {
                    (Some(*oid as i64), "submitted", None, None)
                }
                Some(OrderStatusEntry::Filled { oid, total_sz, .. }) => {
                    (Some(*oid as i64), "filled", None, Some(total_sz.clone()))
                }
                Some(OrderStatusEntry::Error(e)) => (None, "failed", Some(e.clone()), None),
                None => (None, "failed", Some("no status returned".into()), None),
            };
            Ok(SignedSubmitResult {
                cloid: cloid.to_string(),
                oid,
                status: status.into(),
                filled_size_usd: filled,
                err_msg: err,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: cloid.to_string(),
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
        }),
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
        c: Some(p.cloid.clone()),
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
            Ok(SignedSubmitResult {
                cloid: p.cloid,
                oid,
                status: status.into(),
                filled_size_usd: None,
                err_msg: err,
            })
        }
        Err(e) => Ok(SignedSubmitResult {
            cloid: p.cloid,
            oid: None,
            status: "failed".into(),
            filled_size_usd: None,
            err_msg: Some(format!("{e}")),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hl_info::InfoError;
    use async_trait::async_trait;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use std::sync::Mutex;

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
    }

    impl FakeInfo {
        fn returning(r: Result<LivePosition, InfoError>) -> Arc<Self> {
            Arc::new(Self {
                result: Mutex::new(Some(r)),
                last_call: Mutex::new(None),
            })
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
