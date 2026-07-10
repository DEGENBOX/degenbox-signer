//! Push-based MANUAL-intent stream.
//!
//! Fourth sibling of `ws_stream` (preset-matched buy signals),
//! `sell_stream` (TP/SL sell triggers) and `copy_stream` (copy-trade
//! execution): subscribes to `trading.intent.{user_id}` and yields
//! decoded [`ManualIntentEvent`]s for intents a USER created from the
//! web dashboard (`POST /api/trading/intents`, `action = "created"`).
//!
//! Unlike the copy/sell streams, a manual intent already exists as a
//! `trading_intents` row when the event fires — the consumer must NOT
//! create a second one. It reuses the EXISTING row: it builds + signs a
//! swap and submits to `POST /api/trading/intents/{id}/submit` (the same
//! primitive the local daemon `/swap` handler uses when the web UI
//! passes `intentId`). The gateway's atomic claim
//! (`claim_intent_for_submit`) is the money-safety dedup authority, so a
//! WS redelivery — or a second signer on another device — can never
//! double-fill: the loser gets a benign 409.
//!
//! `trading.intent.{user}` also carries `action = "status"` events
//! (submit / fill / cancel transitions) which have NO intent body; those
//! decode to `None` and are ignored here. Every intent the SIGNER itself
//! creates (copy / TP-SL / bot) is ALSO published on this subject at
//! create time — the consumer re-reads authoritative status
//! (`GET /api/trading/intents/{id}`) before acting so it only fills rows
//! that are genuinely still `pending`, exactly like the `mock_signer`'s
//! "grab pending intents" pattern.
//!
//! Wire format + reconnect strategy mirror `sell_stream` / `copy_stream`
//! so the operator's mental model carries over.

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
pub enum IntentStreamError {
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

/// Subset of `module-trading::domain::IntentRow` needed to fill a manual
/// intent. Re-declared (like `CopyExecEvent` / `SellNeededEvent`) so
/// signer-core stays decoupled from the backend crate. Unknown fields
/// on the wire are ignored by serde, so a gateway that adds columns
/// stays compatible.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ManualIntentEvent {
    pub id: Uuid,
    pub owner_user_id: Uuid,
    /// "buy" | "sell".
    pub side: String,
    pub input_mint: String,
    pub output_mint: String,
    /// Buy: SOL lamports to spend (input = WSOL). Sell: token base units
    /// to sell (input = the token).
    pub amount_in_lamports: i64,
    pub slippage_bps: i32,
    /// `pending` at create time; re-checked authoritatively before fill.
    pub status: String,
    pub expires_at: DateTime<Utc>,
    /// Set when the intent was created by the signer executing a
    /// copy-trade event — NEVER a manual intent. Present here so the
    /// consumer can skip it without a round-trip.
    #[serde(default)]
    pub copy_config_id: Option<Uuid>,
    /// Set when the intent belongs to an auto-buy bot session — NEVER a
    /// manual intent.
    #[serde(default)]
    pub bot_session_id: Option<String>,
    /// `trading_clients.id` the intent is scoped to (multi-client). The
    /// wire carries no wallet pubkey for manual intents, so the consumer
    /// can only safely map `None` (→ the primary wallet).
    #[serde(default)]
    pub client_id: Option<Uuid>,
}

impl ManualIntentEvent {
    /// A manual intent carries neither a copy-config nor a bot-session
    /// tag. Signer-created intents (copy / TP-SL / bot) always carry one
    /// or the other, so this cheap pre-filter skips them before the
    /// authoritative status re-check.
    pub fn is_untagged(&self) -> bool {
        self.copy_config_id.is_none() && self.bot_session_id.is_none()
    }
}

/// Inbound frame shapes from the gateway WS multiplexer — same wire
/// format as `sell_stream` / `copy_stream` (duplicated locally, same
/// rationale).
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

/// NATS envelope wrapper — trading events arrive with the publisher
/// payload nested under `payload`.
#[derive(Debug, Deserialize)]
struct IntentEnvelope {
    payload: IntentActionFrame,
}

/// The `publish_intent` / `publish_intent_status` payload shape:
/// `{ action, intent? }`. Only `action = "created"` carries an intent
/// body; `"status"` (submit/fill/cancel) does not.
#[derive(Debug, Deserialize)]
struct IntentActionFrame {
    action: String,
    #[serde(default)]
    intent: Option<ManualIntentEvent>,
}

/// Open the manual-intent push stream with a FIXED credential — legacy
/// CLI behaviour where the token lives as long as the process. New
/// hosts with refreshable auth should use [`spawn_intent_subscriber_with`].
/// Drop the receiver to stop the reconnect loop.
pub async fn spawn_intent_subscriber(
    gateway_base: String,
    token: String,
    user_id: Uuid,
) -> Result<mpsc::Receiver<ManualIntentEvent>, IntentStreamError> {
    build_ws_url(&gateway_base, &token)?;
    spawn_intent_subscriber_with(fixed_token_provider(gateway_base, token), user_id, None).await
}

/// Open the manual-intent push stream, re-resolving credentials through
/// `provider` on EVERY reconnect attempt (audit H1: a captured token
/// expires after ~24 h and the loop would otherwise 401 forever).
/// `health` (optional) receives [`StreamHealth`] events so the host can
/// surface auth-expiry truthfully. `user_id` must be the authenticated
/// user's UUID (the WS multiplexer rejects wildcards on this subject).
/// Drop the receiver to stop the reconnect loop.
pub async fn spawn_intent_subscriber_with(
    provider: TokenProvider,
    user_id: Uuid,
    health: Option<StreamHealthSink>,
) -> Result<mpsc::Receiver<ManualIntentEvent>, IntentStreamError> {
    let (tx, rx) = mpsc::channel::<ManualIntentEvent>(64);
    let subject = format!("trading.intent.{user_id}");

    tokio::spawn(async move {
        const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        /// Auth failures need a human (re-login) — probe slow + steady
        /// instead of hammering the gateway with doomed upgrades.
        const AUTH_RETRY: Duration = Duration::from_secs(30);
        let mut backoff = INITIAL_BACKOFF;
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
                tracing::debug!("intent subscriber: receiver dropped, exiting");
                return;
            }
            if subscribed.swap(false, std::sync::atomic::Ordering::Relaxed) {
                backoff = INITIAL_BACKOFF;
            }
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
                        tracing::debug!("intent subscriber inner loop exited cleanly");
                        return;
                    }
                    Err(IntentStreamError::Unauthorized(status)) => {
                        tracing::warn!(
                            status,
                            %subject,
                            "intent subscriber: gateway rejected credentials — \
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
                            "intent subscriber error, reconnecting"
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
                        "intent subscriber: no usable credentials, retrying"
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
    tx: &mpsc::Sender<ManualIntentEvent>,
    report: &impl Fn(StreamHealth),
) -> Result<(), IntentStreamError> {
    let req = url
        .as_str()
        .into_client_request()
        .map_err(|e| IntentStreamError::BadUrl(e.to_string()))?;
    let (mut ws, _resp) = connect_async(req).await.map_err(|e| {
        match http_reject_status(&e).filter(|s| is_auth_status(*s)) {
            Some(status) => IntentStreamError::Unauthorized(status),
            None => IntentStreamError::Connect(e.to_string()),
        }
    })?;

    let sub_frame = serde_json::json!({
        "type": "subscribe",
        "subjects": [subject],
    })
    .to_string();
    ws.send(Message::Text(sub_frame))
        .await
        .map_err(|e| IntentStreamError::Send(e.to_string()))?;

    tracing::info!(%subject, "intent subscriber: subscribed");
    report(StreamHealth::Subscribed);

    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Half-open guard — see copy_stream.rs: >75s of inbound silence
    // means the link is dead under us; bail so the outer loop reconnects.
    const IDLE_DEADLINE: Duration = Duration::from_secs(75);
    let mut last_rx = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                if last_rx.elapsed() > IDLE_DEADLINE {
                    return Err(IntentStreamError::Connect(
                        "no inbound traffic for 75s — link presumed dead".into(),
                    ));
                }
                let ping = serde_json::json!({"type": "ping"}).to_string();
                ws.send(Message::Text(ping))
                    .await
                    .map_err(|e| IntentStreamError::Send(e.to_string()))?;
            }
            msg = ws.next() => {
                last_rx = tokio::time::Instant::now();
                let Some(msg) = msg else { break };
                let msg = msg.map_err(|e| IntentStreamError::Send(e.to_string()))?;
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
                            .map_err(|e| IntentStreamError::Send(e.to_string()))?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        return Err(IntentStreamError::Connect("server closed".into()));
                    }
                }
            }
        }
    }
    Err(IntentStreamError::Connect("stream ended".into()))
}

/// Extract a `ManualIntentEvent` from a raw WS text frame. `None` for
/// non-matching subjects, decode errors, non-msg frames, or any
/// `action` other than `"created"` (e.g. `"status"` transitions, which
/// carry no intent body).
fn handle_frame(text: &str, expected_subject: &str) -> Option<ManualIntentEvent> {
    let frame: InboundFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(?e, "intent stream frame decode failed");
            return None;
        }
    };
    match frame {
        InboundFrame::Msg { subject, data } => {
            if subject != expected_subject {
                tracing::debug!(%subject, %expected_subject, "ignoring unexpected subject");
                return None;
            }
            // Tolerate both shapes: enveloped (current) + bare payload.
            let action_frame = serde_json::from_value::<IntentEnvelope>(data.clone())
                .map(|e| e.payload)
                .or_else(|_| serde_json::from_value::<IntentActionFrame>(data))
                .ok()?;
            if action_frame.action != "created" {
                // "status" (submit/fill/cancel) — no intent body, ignore.
                return None;
            }
            action_frame.intent
        }
        InboundFrame::Pong => None,
        InboundFrame::Error { msg } => {
            tracing::warn!(%msg, "gateway ws error frame (intent stream)");
            None
        }
    }
}

fn build_ws_url(gateway_base: &str, token: &str) -> Result<String, IntentStreamError> {
    let parsed = Url::parse(gateway_base).map_err(|e| IntentStreamError::BadUrl(e.to_string()))?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(IntentStreamError::BadUrl(format!(
                "unsupported scheme: {other}"
            )))
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| IntentStreamError::BadUrl("no host".into()))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    Ok(format!("{scheme}://{host}{port}/ws?token={token_enc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_intent() -> ManualIntentEvent {
        ManualIntentEvent {
            id: Uuid::nil(),
            owner_user_id: Uuid::nil(),
            side: "buy".into(),
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_in_lamports: 50_000_000,
            slippage_bps: 100,
            status: "pending".into(),
            expires_at: Utc::now(),
            copy_config_id: None,
            bot_session_id: None,
            client_id: None,
        }
    }

    const SUBJECT: &str = "trading.intent.00000000-0000-0000-0000-000000000000";

    #[test]
    fn handle_frame_extracts_created_intent_enveloped() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": SUBJECT,
            "data": { "payload": { "action": "created", "intent": dummy_intent() } }
        });
        let r = handle_frame(&frame.to_string(), SUBJECT).expect("yields intent");
        assert_eq!(r.side, "buy");
        assert_eq!(r.amount_in_lamports, 50_000_000);
        assert!(r.is_untagged());
    }

    #[test]
    fn handle_frame_extracts_created_intent_bare() {
        // Tolerance path: a future publisher might drop the envelope.
        let frame = serde_json::json!({
            "type": "msg",
            "subject": SUBJECT,
            "data": { "action": "created", "intent": dummy_intent() }
        });
        let r = handle_frame(&frame.to_string(), SUBJECT).expect("yields intent");
        assert_eq!(r.output_mint, dummy_intent().output_mint);
    }

    #[test]
    fn handle_frame_ignores_status_action() {
        // `publish_intent_status` shape — no intent body.
        let frame = serde_json::json!({
            "type": "msg",
            "subject": SUBJECT,
            "data": { "payload": {
                "action": "status",
                "intent_id": Uuid::nil(),
                "status": "submitted"
            } }
        });
        assert!(handle_frame(&frame.to_string(), SUBJECT).is_none());
    }

    #[test]
    fn handle_frame_skips_subject_mismatch_and_garbage() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "trading.intent.other-user",
            "data": { "payload": { "action": "created", "intent": dummy_intent() } }
        });
        assert!(handle_frame(&frame.to_string(), SUBJECT).is_none());
        assert!(handle_frame("{not json", "x").is_none());
        let err = serde_json::json!({ "type": "error", "msg": "rate limited" });
        assert!(handle_frame(&err.to_string(), "x").is_none());
    }

    #[test]
    fn untagged_predicate_distinguishes_manual_from_signer_created() {
        let mut evt = dummy_intent();
        assert!(evt.is_untagged(), "no tags = manual");
        evt.copy_config_id = Some(Uuid::nil());
        assert!(!evt.is_untagged(), "copy tag = signer-created");
        let mut evt = dummy_intent();
        evt.bot_session_id = Some("s1".into());
        assert!(!evt.is_untagged(), "bot session = signer-created");
    }

    #[test]
    fn manual_intent_wire_ignores_unknown_fields() {
        // A real gateway IntentRow carries many more columns; serde must
        // ignore them and still decode our subset.
        let frame = serde_json::json!({
            "type": "msg",
            "subject": SUBJECT,
            "data": { "payload": { "action": "created", "intent": {
                "id": Uuid::nil(),
                "owner_user_id": Uuid::nil(),
                "side": "sell",
                "input_mint": "TokenMint1111",
                "output_mint": "So11111111111111111111111111111111111111112",
                "amount_in_lamports": 123_456_789,
                "slippage_bps": 250,
                "submit_mode": "falcon_jito",
                "tip_lamports": 1_000_000,
                "preset_id": null,
                "bot_session_id": null,
                "quote_snapshot": { "route": "jupiter" },
                "expires_at": Utc::now(),
                "status": "pending",
                "completed_at": null,
                "created_at": Utc::now(),
                "client_token": "dbx:swap:x",
                "copy_config_id": null,
                "client_id": null
            } } }
        });
        let r = handle_frame(&frame.to_string(), SUBJECT).expect("decodes subset");
        assert_eq!(r.side, "sell");
        assert_eq!(r.amount_in_lamports, 123_456_789);
        assert_eq!(r.slippage_bps, 250);
        assert!(r.is_untagged());
    }

    #[test]
    fn build_ws_url_schemes() {
        assert_eq!(
            build_ws_url("https://api.degenbox.io", "tok").unwrap(),
            "wss://api.degenbox.io/ws?token=tok"
        );
        assert_eq!(
            build_ws_url("http://localhost:8090", "tok").unwrap(),
            "ws://localhost:8090/ws?token=tok"
        );
        assert!(build_ws_url("ftp://x", "t").is_err());
    }
}
