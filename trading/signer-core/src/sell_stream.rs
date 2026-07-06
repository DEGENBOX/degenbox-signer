//! Push-based sell-needed stream for TP/SL auto-exit.
//!
//! Parallel to `ws_stream` (which feeds `BotEngine::handle_one` with
//! matched alpha signals), this module subscribes to
//! `trading.sell.needed.{user_id}` and yields decoded
//! [`SellNeededEvent`]s. The CLI / daemon consumer calls
//! `BotEngine::execute_sell(mint, token_amount_raw, ...)` for each.
//!
//! ## Why a separate module
//!
//! The two streams have different semantics:
//!
//! - `ws_stream` is subscribed by **preset-id** (`alpha.signals.matched.{id}`)
//!   and yields `Signal` (token to *buy*).
//! - `sell_stream` is subscribed by **user-id** (`trading.sell.needed.{uid}`)
//!   — the WS multiplexer rejects wildcards on this subject, so the
//!   subscriber must know its own user-id (fetched from `/auth/me`).
//!
//! Wire format + reconnect strategy mirror `ws_stream` so both can
//! share the operator's mental model.

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
pub enum SellStreamError {
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

/// TP/SL trigger label. Mirrors `module-trading::targets::TriggerKind`
/// — duplicated here so signer-core doesn't depend on the backend
/// crate (preserves the audit-bare workspace boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerKind {
    Tp,
    Sl,
}

/// Payload mirrored from `module-trading::targets::SellNeededEvent`.
/// The watcher publishes one of these on
/// `trading.sell.needed.{owner_user_id}` whenever a target trips. The
/// signer consumer parses `token_amount_raw` into a `u64` and hands
/// the (mint, amount) tuple to `BotEngine::execute_sell`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SellNeededEvent {
    pub target_id: Uuid,
    /// Which ladder LEG fired (multi-point TP/SL). `None` on payloads
    /// from pre-ladder gateways — `serde(default)` keeps old and new
    /// wire shapes mutually compatible. Display/telemetry only on the
    /// signer side; the sell executes off `token_amount_raw` verbatim.
    #[serde(default)]
    pub leg_id: Option<Uuid>,
    pub owner_user_id: Uuid,
    pub mint: String,
    /// Raw base-unit amount to sell, serialised as a string because
    /// `u128`/`numeric(40,0)` doesn't fit in JSON's `Number`. The
    /// consumer parses to whatever integer width its sell builder
    /// needs (u64 today for PumpFun; could lift to u128 for high-
    /// decimal tokens).
    pub token_amount_raw: String,
    pub trigger_kind: TriggerKind,
    /// JSON-serialised price (numeric → string on the wire). Kept as
    /// `String` here so signer-core doesn't need to drag in
    /// `rust_decimal` for what is a display-only field on this side.
    pub triggered_at_price_usd: String,
    pub triggered_at: DateTime<Utc>,
    /// Raydium AMM v4 pool address for the token, when known. Populated
    /// server-side from `alpha_tokens.pool.pair_address` for tokens
    /// whose `factory_id` is `"raydium"` or `"raydium_amm_v4"`. `None`
    /// for PumpFun / Jupiter tokens. Forwarded to `execute_sell` as
    /// `amm_hint` so native Raydium sells skip the extra RPC hop.
    #[serde(default)]
    pub amm_address: Option<String>,
    /// EXECUTOR identity (multi-client gateways) — the `trading_clients`
    /// row resolved from the most recent client-tagged buy intent for
    /// (owner, mint). `None` = legacy → the PRIMARY engine executes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<Uuid>,
    /// EXECUTOR wallet pubkey (base58). Routing rule:
    /// [`crate::copy_stream::wallet_event_is_mine`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet_pubkey: Option<String>,
}

/// Inbound frame shapes from the gateway WS multiplexer. Same wire
/// format as `ws_stream`; duplicated locally so neither module has to
/// import the other's frame types.
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

/// NATS envelope wrapper. Trading events come through with the
/// publisher payload nested under `payload`.
#[derive(Debug, Deserialize)]
struct SellNeededEnvelope {
    payload: SellNeededEvent,
}

/// Open a single sell-needed push stream with a FIXED credential —
/// legacy CLI behaviour where the token lives as long as the process.
/// New hosts with refreshable auth should use
/// [`spawn_sell_subscriber_with`]. Drop the receiver to stop the
/// reconnect loop.
pub async fn spawn_sell_subscriber(
    gateway_base: String,
    token: String,
    user_id: Uuid,
) -> Result<mpsc::Receiver<SellNeededEvent>, SellStreamError> {
    // Fail fast on an unusable base URL (legacy contract: the caller
    // gets the BadUrl error synchronously, not via silent retries).
    build_ws_url(&gateway_base, &token)?;
    spawn_sell_subscriber_with(fixed_token_provider(gateway_base, token), user_id, None).await
}

/// Open a single sell-needed push stream, re-resolving credentials
/// through `provider` on EVERY reconnect attempt (audit H1: a captured
/// token expires after ~24 h and the loop would otherwise 401 forever).
/// `health` (optional) receives [`StreamHealth`] events so the host can
/// surface auth-expiry truthfully. `user_id` must be the authenticated
/// user's UUID — typically fetched via `RelayClient::fetch_user_id` at
/// startup. Drop the receiver to stop the reconnect loop.
pub async fn spawn_sell_subscriber_with(
    provider: TokenProvider,
    user_id: Uuid,
    health: Option<StreamHealthSink>,
) -> Result<mpsc::Receiver<SellNeededEvent>, SellStreamError> {
    let (tx, rx) = mpsc::channel::<SellNeededEvent>(64);
    let subject = format!("trading.sell.needed.{user_id}");

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
                tracing::debug!("sell subscriber: receiver dropped, exiting");
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
                        tracing::debug!("sell subscriber inner loop exited cleanly");
                        return;
                    }
                    Err(SellStreamError::Unauthorized(status)) => {
                        tracing::warn!(
                            status,
                            %subject,
                            "sell subscriber: gateway rejected credentials — \
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
                            "sell subscriber error, reconnecting"
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
                        "sell subscriber: no usable credentials, retrying"
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
    tx: &mpsc::Sender<SellNeededEvent>,
    report: &impl Fn(StreamHealth),
) -> Result<(), SellStreamError> {
    let req = url
        .as_str()
        .into_client_request()
        .map_err(|e| SellStreamError::BadUrl(e.to_string()))?;
    let (mut ws, _resp) = connect_async(req).await.map_err(|e| {
        match http_reject_status(&e).filter(|s| is_auth_status(*s)) {
            Some(status) => SellStreamError::Unauthorized(status),
            None => SellStreamError::Connect(e.to_string()),
        }
    })?;

    let sub_frame = serde_json::json!({
        "type": "subscribe",
        "subjects": [subject],
    })
    .to_string();
    ws.send(Message::Text(sub_frame))
        .await
        .map_err(|e| SellStreamError::Send(e.to_string()))?;

    tracing::info!(%subject, "sell subscriber: subscribed");
    report(StreamHealth::Subscribed);

    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Half-open guard — see copy_stream.rs: the gateway pings every 30s,
    // so >75s of inbound silence means the link is dead under us; bail
    // so the outer loop reconnects instead of consuming silence forever.
    const IDLE_DEADLINE: Duration = Duration::from_secs(75);
    let mut last_rx = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                if last_rx.elapsed() > IDLE_DEADLINE {
                    return Err(SellStreamError::Connect(
                        "no inbound traffic for 75s — link presumed dead".into(),
                    ));
                }
                let ping = serde_json::json!({"type": "ping"}).to_string();
                ws.send(Message::Text(ping))
                    .await
                    .map_err(|e| SellStreamError::Send(e.to_string()))?;
            }
            msg = ws.next() => {
                last_rx = tokio::time::Instant::now();
                let Some(msg) = msg else { break };
                let msg = msg.map_err(|e| SellStreamError::Send(e.to_string()))?;
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
                            .map_err(|e| SellStreamError::Send(e.to_string()))?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        return Err(SellStreamError::Connect("server closed".into()));
                    }
                }
            }
        }
    }
    Err(SellStreamError::Connect("stream ended".into()))
}

/// Extract a `SellNeededEvent` from a raw WS text frame. Returns
/// `None` for non-matching subjects, decode errors, or non-msg frames.
/// Extracted for testing without spinning up a server.
fn handle_frame(text: &str, expected_subject: &str) -> Option<SellNeededEvent> {
    let frame: InboundFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(?e, "sell stream frame decode failed");
            return None;
        }
    };
    match frame {
        InboundFrame::Msg { subject, data } => {
            if subject != expected_subject {
                tracing::debug!(%subject, %expected_subject, "ignoring unexpected subject");
                return None;
            }
            // Tolerate both shapes: bare payload (older publishers) +
            // enveloped (current). Try envelope first, fall back to
            // direct deserialise.
            if let Ok(env) = serde_json::from_value::<SellNeededEnvelope>(data.clone()) {
                return Some(env.payload);
            }
            match serde_json::from_value::<SellNeededEvent>(data) {
                Ok(evt) => Some(evt),
                Err(e) => {
                    tracing::warn!(?e, %expected_subject, "sell-needed payload decode failed");
                    None
                }
            }
        }
        InboundFrame::Pong => None,
        InboundFrame::Error { msg } => {
            tracing::warn!(%msg, "gateway ws error frame (sell stream)");
            None
        }
    }
}

fn build_ws_url(gateway_base: &str, token: &str) -> Result<String, SellStreamError> {
    let parsed = Url::parse(gateway_base).map_err(|e| SellStreamError::BadUrl(e.to_string()))?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(SellStreamError::BadUrl(format!(
                "unsupported scheme: {other}"
            )))
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| SellStreamError::BadUrl("no host".into()))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    Ok(format!("{scheme}://{host}{port}/ws?token={token_enc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_event() -> SellNeededEvent {
        SellNeededEvent {
            target_id: Uuid::nil(),
            leg_id: Some(Uuid::nil()),
            owner_user_id: Uuid::nil(),
            mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            token_amount_raw: "1000000000".into(),
            trigger_kind: TriggerKind::Tp,
            triggered_at_price_usd: "1.50".into(),
            triggered_at: Utc::now(),
            amm_address: None,
            client_id: None,
            wallet_pubkey: None,
        }
    }

    #[test]
    fn sell_needed_event_without_leg_id_still_parses() {
        // Pre-ladder gateway payload (no leg_id) must decode — the
        // field is serde(default) for wire compatibility.
        let j = serde_json::json!({
            "target_id": Uuid::nil(),
            "owner_user_id": Uuid::nil(),
            "mint": "M",
            "token_amount_raw": "5",
            "trigger_kind": "sl",
            "triggered_at_price_usd": "0.5",
            "triggered_at": Utc::now(),
        });
        let evt: SellNeededEvent = serde_json::from_value(j).unwrap();
        assert_eq!(evt.leg_id, None);
        // Executor identity is equally optional (pre-multi-client wire).
        assert_eq!(evt.client_id, None);
        assert_eq!(evt.wallet_pubkey, None);
    }

    #[test]
    fn executor_identity_omitted_when_none_and_round_trips_when_set() {
        let evt = dummy_event();
        let j = serde_json::to_value(&evt).unwrap();
        assert!(
            j.get("wallet_pubkey").is_none(),
            "None must be omitted on the wire"
        );
        assert!(j.get("client_id").is_none());
        let mut evt = dummy_event();
        evt.wallet_pubkey = Some("Wsol111".into());
        evt.client_id = Some(Uuid::nil());
        let j = serde_json::to_value(&evt).unwrap();
        let back: SellNeededEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.wallet_pubkey.as_deref(), Some("Wsol111"));
        assert_eq!(back.client_id, Some(Uuid::nil()));
    }

    #[test]
    fn build_ws_url_https_to_wss() {
        let u = build_ws_url("https://api.degenbox.io", "tok").unwrap();
        assert_eq!(u, "wss://api.degenbox.io/ws?token=tok");
    }

    #[test]
    fn build_ws_url_http_to_ws() {
        let u = build_ws_url("http://localhost:8080", "tok").unwrap();
        assert_eq!(u, "ws://localhost:8080/ws?token=tok");
    }

    #[test]
    fn build_ws_url_rejects_bad_scheme() {
        assert!(build_ws_url("ftp://x", "t").is_err());
    }

    #[test]
    fn handle_frame_extracts_enveloped_payload() {
        let evt = dummy_event();
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.sell.needed.00000000-0000-0000-0000-000000000000",
            "data": { "payload": evt }
        });
        let r = handle_frame(
            &frame.to_string(),
            "trading.sell.needed.00000000-0000-0000-0000-000000000000",
        )
        .expect("yields event");
        assert_eq!(r.mint, evt.mint);
        assert_eq!(r.token_amount_raw, "1000000000");
        assert_eq!(r.trigger_kind, TriggerKind::Tp);
    }

    #[test]
    fn handle_frame_extracts_bare_payload() {
        // Tolerance path: a future publisher might drop the envelope.
        let evt = dummy_event();
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.sell.needed.00000000-0000-0000-0000-000000000000",
            "data": evt
        });
        let r = handle_frame(
            &frame.to_string(),
            "trading.sell.needed.00000000-0000-0000-0000-000000000000",
        )
        .expect("yields event");
        assert_eq!(r.mint, evt.mint);
    }

    #[test]
    fn handle_frame_skips_subject_mismatch() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.sell.needed.other-user",
            "data": { "payload": dummy_event() }
        });
        let r = handle_frame(
            &frame.to_string(),
            "trading.sell.needed.00000000-0000-0000-0000-000000000000",
        );
        assert!(r.is_none());
    }

    #[test]
    fn handle_frame_returns_none_on_error_frame() {
        let frame = serde_json::json!({ "type": "error", "msg": "rate limited" });
        assert!(handle_frame(&frame.to_string(), "x").is_none());
    }

    #[test]
    fn handle_frame_returns_none_on_garbage() {
        assert!(handle_frame("{not json", "x").is_none());
    }

    #[test]
    fn sell_needed_event_round_trips_through_json() {
        let evt = dummy_event();
        let j = serde_json::to_value(&evt).unwrap();
        let back: SellNeededEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back.token_amount_raw, evt.token_amount_raw);
        assert_eq!(back.trigger_kind, evt.trigger_kind);
    }

    #[test]
    fn trigger_kind_serialises_lowercase() {
        let j = serde_json::to_string(&TriggerKind::Sl).unwrap();
        assert_eq!(j, r#""sl""#);
        let j = serde_json::to_string(&TriggerKind::Tp).unwrap();
        assert_eq!(j, r#""tp""#);
    }
}
