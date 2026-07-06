//! Push-based copy-trade execution stream.
//!
//! Third sibling of `ws_stream` (preset-matched buy signals) and
//! `sell_stream` (TP/SL sell triggers): subscribes to
//! `trading.copy.exec.{user_id}` and yields decoded [`CopyExecEvent`]s
//! published by the gateway's Solana copy-trade engine
//! (`module-trading::copy_trade`). The consumer dispatches
//! `BotEngine::execute_buy` (buys) / `BotEngine::execute_sell`
//! (mirror-sells) — same execution pipeline as every other trade.
//!
//! Buy sizing is finalised CLIENT-side via [`resolve_buy_lamports`]:
//! fixed-mode events arrive pre-sized (and pre-clamped) by the server;
//! pct-of-balance events are resolved against the signer wallet's live
//! balance — the 4-asset cash basis (SOL + wSOL + USDC + USDT, valued
//! via the event's `sol_price_usd`, D8) when the gateway forwards a
//! spot, else the bare SOL balance — then clamped to the
//! server-supplied remaining headroom (`max_spend_lamports`, which
//! folds the per-config copy budget + position cap + per-trade cap).
//!
//! Wire format + reconnect strategy mirror `sell_stream` so the
//! operator's mental model carries over.

use crate::stream_auth::{
    fixed_token_provider, http_reject_status, is_auth_status, StreamHealth, StreamHealthSink,
    TokenProvider,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use url::Url;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum CopyStreamError {
    #[error("invalid gateway url: {0}")]
    BadUrl(String),
    #[error("ws connect: {0}")]
    Connect(String),
    /// The gateway rejected the upgrade with an auth status (401/403):
    /// the presented token is expired, revoked, or junk.
    #[error("ws unauthorized: gateway rejected the token with HTTP {0}")]
    Unauthorized(u16),
    #[error("ws send: {0}")]
    Send(String),
}

/// Payload mirrored from `module-trading::copy_trade::CopyExecEvent`.
/// Re-declared (like `SellNeededEvent`) so signer-core stays decoupled
/// from the backend crate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CopyExecEvent {
    /// Backend `sol_copy_trade_intents.id` — feed it to the engine's
    /// dedupe so a WS redelivery can't double-execute.
    pub intent_id: Uuid,
    pub config_id: Uuid,
    pub user_id: Uuid,
    /// Source wallet being copied (display / logs).
    pub wallet_address: String,
    pub source_tx_hash: String,
    pub mint: String,
    /// "buy" | "sell".
    pub side: String,
    /// 0 = fixed SOL, 1 = % of live balance.
    pub sizing_mode: i16,
    #[serde(default)]
    pub sol_lamports: Option<i64>,
    #[serde(default)]
    pub pct_of_balance_bps: Option<i32>,
    /// SOL/USD spot at gateway emit time. When present on a pct-mode
    /// buy, the balance denominator is the 4-asset CASH basis (native
    /// SOL + wSOL + USDC + USDT valued in SOL terms, D8) instead of the
    /// bare SOL balance. `None` (legacy gateway / no spot available)
    /// keeps the SOL-only denominator.
    #[serde(default)]
    pub sol_price_usd: Option<f64>,
    /// Remaining cumulative position-cap headroom; the resolved size
    /// MUST be clamped to this. `None` = uncapped.
    #[serde(default)]
    pub max_spend_lamports: Option<i64>,
    pub slippage_bps: i32,
    /// Mirror-sell size in raw token base units (string-encoded).
    #[serde(default)]
    pub token_amount_raw: Option<String>,
    /// Raydium AMM v4 pool hint — forward to the engine's `amm_hint`.
    #[serde(default)]
    pub amm_address: Option<String>,
    pub at: DateTime<Utc>,
    /// EXECUTOR identity (multi-client gateways): the `trading_clients`
    /// row this copy config is bound to. `None` on legacy/unbound
    /// configs. Omitted on the wire when absent so old consumers keep
    /// parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<Uuid>,
    /// EXECUTOR wallet pubkey (base58) the event must execute on. A
    /// per-wallet engine executes ONLY events whose `wallet_pubkey`
    /// matches its own wallet; events without the field belong to the
    /// designated PRIMARY engine (see [`wallet_event_is_mine`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet_pubkey: Option<String>,
}

/// Should the engine for `my_wallet` execute an event stamped with
/// `event_wallet`? The single routing rule for BOTH Sol streams
/// (`trading.copy.exec.*` / `trading.sell.needed.*`):
///
/// - stamped events execute ONLY on the exactly-matching wallet
///   (base58 is case-sensitive — exact comparison);
/// - unstamped (legacy) events execute ONLY on the designated primary,
///   so nothing double-executes during the gateway rollout.
///
/// Pure — the dispatcher loops engines with this predicate; vault
/// wallets are unique and exactly one is primary, so at most one engine
/// claims any event.
pub fn wallet_event_is_mine(
    event_wallet: Option<&str>,
    my_wallet: &str,
    i_am_primary: bool,
) -> bool {
    match event_wallet {
        Some(w) => w == my_wallet,
        None => i_am_primary,
    }
}

/// Value the 4-asset "cash" denominator (D8) in LAMPORT equivalents:
/// native SOL + wSOL (both already lamports) + USDC + USDT (6-decimal
/// raw base units, valued at `sol_price_usd`). Pure — unit-testable.
///
/// Fail-closed on a bad price: a non-finite or non-positive
/// `sol_price_usd` returns `None` (the caller must SKIP the buy, never
/// guess a denominator). Missing token accounts are the caller's `0`s —
/// an absent ATA is a real zero balance, not a read failure.
pub fn cash_denominator_lamports(
    sol_lamports: u64,
    wsol_lamports: u64,
    usdc_raw: u64,
    usdt_raw: u64,
    sol_price_usd: f64,
) -> Option<u64> {
    if !sol_price_usd.is_finite() || sol_price_usd <= 0.0 {
        return None;
    }
    // Stables: raw (6 dp) → USD → SOL at spot → lamports (9 dp).
    // f64 is fine here: this is a sizing DENOMINATOR (bps get applied
    // to it), not settlement math, and the 52-bit mantissa covers any
    // realistic cash balance exactly enough.
    let stable_usd = (usdc_raw.saturating_add(usdt_raw)) as f64 / 1e6;
    let stable_lamports = stable_usd / sol_price_usd * 1e9;
    if !stable_lamports.is_finite() || stable_lamports < 0.0 {
        return None;
    }
    let stable_lamports = if stable_lamports >= u64::MAX as f64 {
        u64::MAX
    } else {
        stable_lamports as u64
    };
    Some(
        sol_lamports
            .saturating_add(wsol_lamports)
            .saturating_add(stable_lamports),
    )
}

/// Resolve the final buy size in lamports. Pure — fully unit-testable.
///
///   * mode 0 (fixed): the server already resolved + cap-clamped the
///     size; reject a missing/non-positive value instead of guessing.
///   * mode 1 (pct): `balance × bps / 10000`, minus nothing — fee
///     headroom is the operator's business via the budget caps — then
///     clamped to `max_spend_lamports` when the config carries a cap.
///     `balance_lamports` is whatever denominator the caller resolved
///     (bare SOL, or the 4-asset cash via
///     [`cash_denominator_lamports`]).
///
/// Errors are static strings for log-and-skip handling in the loop.
pub fn resolve_buy_lamports(
    evt: &CopyExecEvent,
    balance_lamports: u64,
) -> Result<u64, &'static str> {
    let raw: u64 = match evt.sizing_mode {
        0 => match evt.sol_lamports {
            Some(v) if v > 0 => v as u64,
            _ => return Err("fixed-mode event without a positive sol_lamports"),
        },
        1 => {
            let bps = match evt.pct_of_balance_bps {
                Some(b) if (1..=10_000).contains(&b) => b as u64,
                _ => return Err("pct-mode event without a valid pct_of_balance_bps"),
            };
            // u128 intermediate: balance (<= ~5.8e17 lamports total
            // supply) × 10_000 stays far below u128::MAX.
            ((balance_lamports as u128 * bps as u128) / 10_000) as u64
        }
        _ => return Err("unknown sizing_mode"),
    };
    let clamped = match evt.max_spend_lamports {
        Some(cap) if cap > 0 => raw.min(cap as u64),
        Some(_) => return Err("non-positive max_spend_lamports"),
        None => raw,
    };
    if clamped == 0 {
        return Err("resolved size is zero");
    }
    Ok(clamped)
}

/// Inbound frame shapes from the gateway WS multiplexer — same wire
/// format as `sell_stream` (duplicated locally, same rationale).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum InboundFrame {
    #[serde(rename = "msg")]
    Msg {
        subject: String,
        data: serde_json::Value,
    },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "error")]
    Error { msg: String },
}

#[derive(Debug, Deserialize)]
struct CopyExecEnvelope {
    payload: CopyExecEvent,
}

/// Open the copy-exec push stream with a FIXED credential — legacy CLI
/// behaviour where the token lives as long as the process. New hosts
/// with refreshable auth should use [`spawn_copy_subscriber_with`].
/// Drop the receiver to stop the reconnect loop.
pub async fn spawn_copy_subscriber(
    gateway_base: String,
    token: String,
    user_id: Uuid,
) -> Result<mpsc::Receiver<CopyExecEvent>, CopyStreamError> {
    // Fail fast on an unusable base URL (legacy contract: the caller
    // gets the BadUrl error synchronously, not via silent retries).
    build_ws_url(&gateway_base, &token)?;
    spawn_copy_subscriber_with(fixed_token_provider(gateway_base, token), user_id, None).await
}

/// Open the copy-exec push stream, re-resolving credentials through
/// `provider` on EVERY reconnect attempt (audit H1: a captured token
/// expires after ~24 h and the loop would otherwise 401 forever).
/// `health` (optional) receives [`StreamHealth`] events so the host can
/// surface auth-expiry truthfully. `user_id` must be the authenticated
/// user's UUID (the WS multiplexer rejects wildcards on this subject).
/// Drop the receiver to stop the reconnect loop.
pub async fn spawn_copy_subscriber_with(
    provider: TokenProvider,
    user_id: Uuid,
    health: Option<StreamHealthSink>,
) -> Result<mpsc::Receiver<CopyExecEvent>, CopyStreamError> {
    let (tx, rx) = mpsc::channel::<CopyExecEvent>(64);
    let subject = format!("trading.copy.exec.{user_id}");

    tokio::spawn(async move {
        const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        /// Auth failures need a human (re-login) — probe slow + steady
        /// instead of hammering the gateway with doomed upgrades.
        const AUTH_RETRY: Duration = Duration::from_secs(30);
        let mut backoff = INITIAL_BACKOFF;
        // Set by run_once's Subscribed report so a long-lived session
        // restarts the backoff ladder after it eventually drops.
        let subscribed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let report = {
            let subscribed = subscribed.clone();
            move |h: StreamHealth| {
                if matches!(h, StreamHealth::Subscribed) {
                    subscribed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(sink) = &health {
                    sink(h);
                }
            }
        };

        loop {
            if tx.is_closed() {
                tracing::debug!("copy subscriber: receiver dropped, exiting");
                return;
            }
            if subscribed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                backoff = INITIAL_BACKOFF;
            }
            // Fresh credentials per attempt — a re-login / token
            // refresh on disk is picked up here without a restart.
            let attempt = match provider().await {
                Ok(auth) => match build_ws_url(&auth.gateway_base, &auth.token) {
                    Ok(url) => Ok(url),
                    Err(e) => Err((format!("bad gateway url: {e}"), false)),
                },
                Err(e) => Err((e, true)),
            };
            let mut auth_failed = false;
            match attempt {
                Ok(ws_url) => match run_once(ws_url, &subject, &tx, &report).await {
                    Ok(()) => {
                        tracing::debug!("copy subscriber inner loop exited cleanly");
                        return;
                    }
                    Err(CopyStreamError::Unauthorized(status)) => {
                        tracing::warn!(
                            status,
                            %subject,
                            "copy subscriber: gateway rejected credentials — \
                             re-resolving on next attempt"
                        );
                        report(StreamHealth::AuthFailed {
                            message: format!("gateway rejected the stream token (HTTP {status})"),
                        });
                        auth_failed = true;
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?e,
                            backoff_ms = backoff.as_millis(),
                            %subject,
                            "copy subscriber error, reconnecting"
                        );
                        report(StreamHealth::Disconnected {
                            message: e.to_string(),
                        });
                    }
                },
                Err((msg, is_auth)) => {
                    tracing::warn!(
                        error = %msg,
                        %subject,
                        "copy subscriber: no usable credentials, retrying"
                    );
                    report(if is_auth {
                        StreamHealth::AuthFailed { message: msg }
                    } else {
                        StreamHealth::Disconnected { message: msg }
                    });
                    auth_failed = is_auth;
                }
            }
            if auth_failed {
                tokio::time::sleep(AUTH_RETRY).await;
                backoff = INITIAL_BACKOFF;
            } else {
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
            }
        }
    });

    Ok(rx)
}

async fn run_once(
    url: String,
    subject: &str,
    tx: &mpsc::Sender<CopyExecEvent>,
    report: &impl Fn(StreamHealth),
) -> Result<(), CopyStreamError> {
    let req = url
        .as_str()
        .into_client_request()
        .map_err(|e| CopyStreamError::BadUrl(e.to_string()))?;
    let (mut ws, _resp) = connect_async(req).await.map_err(|e| {
        match http_reject_status(&e).filter(|s| is_auth_status(*s)) {
            Some(status) => CopyStreamError::Unauthorized(status),
            None => CopyStreamError::Connect(e.to_string()),
        }
    })?;

    let sub_frame = serde_json::json!({
        "type": "subscribe",
        "subjects": [subject],
    })
    .to_string();
    ws.send(Message::Text(sub_frame))
        .await
        .map_err(|e| CopyStreamError::Send(e.to_string()))?;

    tracing::info!(%subject, "copy subscriber: subscribed");
    report(StreamHealth::Subscribed);

    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Half-open guard: the gateway pings every 30s and answers our JSON
    // pings, so a healthy link ALWAYS has inbound traffic. If nothing
    // arrives for this long the TCP link is dead under us (NAT drop,
    // gateway recreate) even though local sends still "succeed" — bail
    // so the outer loop reconnects instead of consuming silence forever.
    const IDLE_DEADLINE: Duration = Duration::from_secs(75);
    let mut last_rx = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                if last_rx.elapsed() > IDLE_DEADLINE {
                    return Err(CopyStreamError::Connect(
                        "no inbound traffic for 75s — link presumed dead".into(),
                    ));
                }
                let ping = serde_json::json!({"type": "ping"}).to_string();
                ws.send(Message::Text(ping))
                    .await
                    .map_err(|e| CopyStreamError::Send(e.to_string()))?;
            }
            msg = ws.next() => {
                last_rx = tokio::time::Instant::now();
                let Some(msg) = msg else { break };
                let msg = msg.map_err(|e| CopyStreamError::Send(e.to_string()))?;
                match msg {
                    Message::Text(t) => {
                        if let Some(evt) = handle_frame(&t, subject) {
                            if tx.send(evt).await.is_err() {
                                return Ok(()); // consumer dropped
                            }
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p))
                            .await
                            .map_err(|e| CopyStreamError::Send(e.to_string()))?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        return Err(CopyStreamError::Connect("server closed".into()));
                    }
                }
            }
        }
    }
    Err(CopyStreamError::Connect("stream ended".into()))
}

/// Extract a `CopyExecEvent` from a raw WS text frame. `None` for
/// non-matching subjects, decode errors, or non-msg frames.
fn handle_frame(text: &str, expected_subject: &str) -> Option<CopyExecEvent> {
    let frame: InboundFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(?e, "copy stream frame decode failed");
            return None;
        }
    };
    match frame {
        InboundFrame::Msg { subject, data } => {
            if subject != expected_subject {
                tracing::debug!(%subject, %expected_subject, "ignoring unexpected subject");
                return None;
            }
            if let Ok(env) = serde_json::from_value::<CopyExecEnvelope>(data.clone()) {
                return Some(env.payload);
            }
            match serde_json::from_value::<CopyExecEvent>(data) {
                Ok(evt) => Some(evt),
                Err(e) => {
                    tracing::warn!(?e, %expected_subject, "copy-exec payload decode failed");
                    None
                }
            }
        }
        InboundFrame::Pong => None,
        InboundFrame::Error { msg } => {
            tracing::warn!(%msg, "gateway ws error frame (copy stream)");
            None
        }
    }
}

fn build_ws_url(gateway_base: &str, token: &str) -> Result<String, CopyStreamError> {
    let parsed = Url::parse(gateway_base).map_err(|e| CopyStreamError::BadUrl(e.to_string()))?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(CopyStreamError::BadUrl(format!(
                "unsupported scheme: {other}"
            )))
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| CopyStreamError::BadUrl("no host".into()))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    Ok(format!("{scheme}://{host}{port}/ws?token={token_enc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy_event(mode: i16) -> CopyExecEvent {
        CopyExecEvent {
            intent_id: Uuid::nil(),
            config_id: Uuid::nil(),
            user_id: Uuid::nil(),
            wallet_address: "7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU".into(),
            source_tx_hash: "sig1".into(),
            mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            side: "buy".into(),
            sizing_mode: mode,
            sol_lamports: (mode == 0).then_some(50_000_000),
            pct_of_balance_bps: (mode == 1).then_some(500), // 5 %
            sol_price_usd: None,
            max_spend_lamports: None,
            slippage_bps: 200,
            token_amount_raw: None,
            amm_address: None,
            at: Utc::now(),
            client_id: None,
            wallet_pubkey: None,
        }
    }

    // ── resolve_buy_lamports ────────────────────────────────────────

    #[test]
    fn fixed_mode_uses_server_size_verbatim() {
        let evt = buy_event(0);
        assert_eq!(resolve_buy_lamports(&evt, 0), Ok(50_000_000));
        // Balance is irrelevant for fixed mode (insufficient funds
        // surface in the pre-sign simulation, not here).
    }

    #[test]
    fn fixed_mode_rejects_missing_or_zero_size() {
        let mut evt = buy_event(0);
        evt.sol_lamports = None;
        assert!(resolve_buy_lamports(&evt, 1).is_err());
        evt.sol_lamports = Some(0);
        assert!(resolve_buy_lamports(&evt, 1).is_err());
    }

    #[test]
    fn pct_mode_scales_with_balance() {
        let evt = buy_event(1); // 5 %
                                // 10 SOL balance → 0.5 SOL.
        assert_eq!(resolve_buy_lamports(&evt, 10_000_000_000), Ok(500_000_000));
        // 1 lamport balance × 5 % floors to zero → rejected.
        assert_eq!(resolve_buy_lamports(&evt, 1), Err("resolved size is zero"));
    }

    #[test]
    fn pct_mode_rejects_bad_spec() {
        let mut evt = buy_event(1);
        evt.pct_of_balance_bps = None;
        assert!(resolve_buy_lamports(&evt, 1_000_000_000).is_err());
        evt.pct_of_balance_bps = Some(0);
        assert!(resolve_buy_lamports(&evt, 1_000_000_000).is_err());
        evt.pct_of_balance_bps = Some(10_001);
        assert!(resolve_buy_lamports(&evt, 1_000_000_000).is_err());
    }

    #[test]
    fn cap_clamps_both_modes() {
        let mut evt = buy_event(0);
        evt.max_spend_lamports = Some(10_000_000);
        assert_eq!(resolve_buy_lamports(&evt, 0), Ok(10_000_000));

        let mut evt = buy_event(1); // 5 % of 100 SOL = 5 SOL
        evt.max_spend_lamports = Some(1_000_000_000); // cap 1 SOL
        assert_eq!(
            resolve_buy_lamports(&evt, 100_000_000_000),
            Ok(1_000_000_000)
        );
    }

    #[test]
    fn pct_mode_full_balance_no_overflow() {
        let mut evt = buy_event(1);
        evt.pct_of_balance_bps = Some(10_000); // 100 %
        assert_eq!(resolve_buy_lamports(&evt, u64::MAX), Ok(u64::MAX));
    }

    #[test]
    fn unknown_mode_rejected() {
        let mut evt = buy_event(0);
        evt.sizing_mode = 7;
        assert!(resolve_buy_lamports(&evt, 1_000_000_000).is_err());
    }

    // ── cash_denominator_lamports (D8 4-asset cash basis) ──────────

    #[test]
    fn cash_denominator_values_all_four_assets() {
        // 2 SOL native + 1 wSOL + 300 USDC + 150 USDT at $150/SOL:
        // stables = $450 = 3 SOL → total 6 SOL.
        let d = cash_denominator_lamports(
            2_000_000_000,
            1_000_000_000,
            300_000_000, // 300 USDC (6 dp)
            150_000_000, // 150 USDT (6 dp)
            150.0,
        );
        assert_eq!(d, Some(6_000_000_000));
    }

    #[test]
    fn cash_denominator_without_stables_is_sol_plus_wsol() {
        let d = cash_denominator_lamports(2_000_000_000, 500_000_000, 0, 0, 150.0);
        assert_eq!(d, Some(2_500_000_000));
    }

    #[test]
    fn cash_denominator_rejects_bad_price_fail_closed() {
        for price in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                cash_denominator_lamports(1_000_000_000, 0, 1_000_000, 0, price),
                None,
                "price {price} must fail closed"
            );
        }
    }

    #[test]
    fn cash_denominator_saturates_instead_of_overflowing() {
        // Absurd stable balance at a dust price → saturates, no panic.
        let d = cash_denominator_lamports(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 0.000001);
        assert_eq!(d, Some(u64::MAX));
    }

    /// End-to-end: the cash denominator feeds mode-1 sizing exactly like
    /// a bare balance would — 5 % of 6 SOL cash = 0.3 SOL.
    #[test]
    fn pct_mode_scales_with_cash_denominator() {
        let evt = buy_event(1); // 5 %
        let cash = cash_denominator_lamports(
            2_000_000_000,
            1_000_000_000,
            300_000_000,
            150_000_000,
            150.0,
        )
        .unwrap();
        assert_eq!(resolve_buy_lamports(&evt, cash), Ok(300_000_000));
    }

    /// Wire compat: a legacy gateway payload without `sol_price_usd`
    /// still decodes (None), and the field round-trips when present.
    #[test]
    fn sol_price_usd_is_wire_compatible() {
        let evt = buy_event(1);
        let j = serde_json::to_value(&evt).unwrap();
        let back: CopyExecEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.sol_price_usd, None);
        let mut evt = buy_event(1);
        evt.sol_price_usd = Some(151.25);
        let j = serde_json::to_value(&evt).unwrap();
        let back: CopyExecEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.sol_price_usd, Some(151.25));
    }

    // ── wire / frame handling ───────────────────────────────────────

    #[test]
    fn handle_frame_extracts_enveloped_payload() {
        let evt = buy_event(0);
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.copy.exec.00000000-0000-0000-0000-000000000000",
            "data": { "payload": evt }
        });
        let r = handle_frame(
            &frame.to_string(),
            "trading.copy.exec.00000000-0000-0000-0000-000000000000",
        )
        .expect("yields event");
        assert_eq!(r.side, "buy");
        assert_eq!(r.sol_lamports, Some(50_000_000));
    }

    #[test]
    fn handle_frame_extracts_bare_payload() {
        let evt = buy_event(1);
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.copy.exec.00000000-0000-0000-0000-000000000000",
            "data": evt
        });
        let r = handle_frame(
            &frame.to_string(),
            "trading.copy.exec.00000000-0000-0000-0000-000000000000",
        )
        .expect("yields event");
        assert_eq!(r.pct_of_balance_bps, Some(500));
    }

    #[test]
    fn handle_frame_skips_subject_mismatch_and_garbage() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.copy.exec.other-user",
            "data": { "payload": buy_event(0) }
        });
        assert!(handle_frame(
            &frame.to_string(),
            "trading.copy.exec.00000000-0000-0000-0000-000000000000",
        )
        .is_none());
        assert!(handle_frame("{not json", "x").is_none());
        let err = serde_json::json!({ "type": "error", "msg": "rate limited" });
        assert!(handle_frame(&err.to_string(), "x").is_none());
    }

    #[test]
    fn sell_event_decodes_with_token_amount() {
        // Pin the backend wire shape for mirror-sells (sol fields
        // omitted entirely — serde defaults must absorb that).
        let j = serde_json::json!({
            "intent_id": Uuid::nil(),
            "config_id": Uuid::nil(),
            "user_id": Uuid::nil(),
            "wallet_address": "w",
            "source_tx_hash": "tx",
            "mint": "M",
            "side": "sell",
            "sizing_mode": 0,
            "slippage_bps": 150,
            "token_amount_raw": "123456789012345678901",
            "at": Utc::now(),
        });
        let evt: CopyExecEvent = serde_json::from_value(j).unwrap();
        assert_eq!(evt.side, "sell");
        assert_eq!(evt.sol_lamports, None);
        assert_eq!(
            evt.token_amount_raw.as_deref(),
            Some("123456789012345678901")
        );
    }

    #[test]
    fn executor_identity_fields_are_wire_compatible() {
        // Legacy payload (no executor identity) still parses…
        let evt = buy_event(0);
        let j = serde_json::to_value(&evt).unwrap();
        assert!(
            j.get("client_id").is_none(),
            "None must be omitted on the wire"
        );
        assert!(j.get("wallet_pubkey").is_none());
        let back: CopyExecEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.wallet_pubkey, None);
        // …and stamped payloads round-trip.
        let mut evt = buy_event(0);
        evt.client_id = Some(Uuid::nil());
        evt.wallet_pubkey = Some("WaLLet111".into());
        let j = serde_json::to_value(&evt).unwrap();
        let back: CopyExecEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.wallet_pubkey.as_deref(), Some("WaLLet111"));
        assert_eq!(back.client_id, Some(Uuid::nil()));
    }

    // ── wallet_event_is_mine: the Sol routing rule ─────────────────

    /// Safety test (b): a legacy (unstamped) event executes on exactly
    /// ONE engine — the primary — across any fleet of engines.
    #[test]
    fn legacy_event_routes_to_exactly_one_engine() {
        let engines = [("W1", true), ("W2", false), ("W3", false)];
        let takers: Vec<&str> = engines
            .iter()
            .filter(|(w, primary)| wallet_event_is_mine(None, w, *primary))
            .map(|(w, _)| *w)
            .collect();
        assert_eq!(takers, vec!["W1"]);
    }

    /// A stamped event executes on exactly the matching engine — even
    /// when the primary is a different wallet.
    #[test]
    fn stamped_event_routes_to_exactly_the_matching_engine() {
        let engines = [("W1", true), ("W2", false), ("W3", false)];
        let takers: Vec<&str> = engines
            .iter()
            .filter(|(w, primary)| wallet_event_is_mine(Some("W2"), w, *primary))
            .map(|(w, _)| *w)
            .collect();
        assert_eq!(takers, vec!["W2"]);
        // Stamp for a wallet this device doesn't hold → NO engine runs it.
        let takers = engines
            .iter()
            .filter(|(w, primary)| wallet_event_is_mine(Some("Welsewhere"), w, *primary))
            .count();
        assert_eq!(takers, 0);
        // Base58 is case-sensitive: a case-mangled stamp must not match.
        assert!(!wallet_event_is_mine(Some("w2"), "W2", false));
    }

    #[test]
    fn build_ws_url_schemes() {
        let u = build_ws_url("https://api.degenbox.io", "tok").unwrap();
        assert_eq!(u, "wss://api.degenbox.io/ws?token=tok");
        let u = build_ws_url("http://localhost:8090", "tok").unwrap();
        assert_eq!(u, "ws://localhost:8090/ws?token=tok");
        assert!(build_ws_url("ftp://x", "t").is_err());
    }
}
