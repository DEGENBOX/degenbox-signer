//! Push-based signal stream for the bot-engine.
//!
//! Connects to the DegenBox gateway's WebSocket multiplexer at
//! `wss://<gateway>/ws?token=<jwt>`, subscribes to
//! `alpha.signals.matched.{preset_id}`, and yields decoded `Signal`s.
//!
//! ## Why WS over direct NATS
//!
//! The signer is a *client* (extension / desktop / CLI behind a user's
//! firewall). The NATS broker isn't publicly exposed — only the
//! gateway is. The gateway already runs a per-user-scoped WS
//! multiplexer (`crates/platform/ws`) that fans out NATS subjects
//! after JWT-checking the user; we reuse that surface.
//!
//! ## Wire shapes
//!
//! Outbound (one frame per subject set):
//!   `{ "type": "subscribe",  "subjects": ["alpha.signals.matched.<id>"] }`
//!
//! Inbound (per matched preset):
//!   `{ "type": "msg",
//!      "subject": "alpha.signals.matched.<id>",
//!      "data": { Envelope wrapping the publisher payload } }`
//!
//! The publisher payload (alpha-scanner filter worker) carries the
//! enriched fields we need to make a trade decision without a DB
//! round-trip: chain id, token_address, market_cap_usd, liquidity_usd,
//! matched_at. Decoded into `Signal` and yielded.
//!
//! ## Reconnect strategy
//!
//! Exponential backoff capped at 30 s. Every reconnect re-issues the
//! subscribe frame for every preset we were tracking. Signal IDs
//! (`call_id`) are deduped by the consumer (`BotEngine::seen`) so a
//! duplicate post-reconnect doesn't fire a second trade.

use crate::bot_engine::Signal;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use url::Url;

#[derive(Debug, Error)]
pub enum WsStreamError {
    #[error("invalid gateway url: {0}")]
    BadUrl(String),
    #[error("ws connect: {0}")]
    Connect(String),
    #[error("ws send: {0}")]
    Send(String),
    #[error("internal channel closed")]
    ChannelClosed,
}

/// Inbound frame from the gateway WS multiplexer.
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

/// Payload published on `alpha.signals.matched.{preset_id}`.
///
/// `data` is the `platform_nats::Envelope` wrapper; the actual fields
/// from the alpha-scanner filter worker live inside `data.payload`.
#[derive(Debug, Deserialize)]
struct MatchedEnvelope {
    payload: MatchedPayload,
}

#[derive(Debug, Deserialize)]
struct MatchedPayload {
    call_id: String,
    preset_id: String,
    #[serde(default)]
    matched_at: Option<DateTime<Utc>>,
    // Enriched fields — present on real publishes. `#[serde(default)]`
    // keeps us compatible with older publishers in case of staggered
    // rollout (the bot just falls back to skipping when missing).
    #[serde(default)]
    chain_id: Option<i16>,
    #[serde(default)]
    token_address: Option<String>,
    /// rust_decimal serializes as a JSON string; parse to f64 lazily.
    #[serde(default)]
    market_cap_usd: Option<String>,
    #[serde(default)]
    liquidity_usd: Option<String>,
    /// `true` when the match came from the backfill path. We skip
    /// those — the bot only acts on live signals.
    #[serde(default)]
    backfill: Option<bool>,
    /// Raydium AMM v4 pool address for the token. Present only when
    /// the server-side filter worker identifies the token's primary
    /// venue as Raydium (`factory_id` = "raydium" or "raydium_amm_v4").
    /// When set, the bot skips the PumpFun bonding-curve lookup and
    /// goes straight to the native Raydium swap path.
    #[serde(default)]
    amm_address: Option<String>,
}

/// Build the matched-payload → `Signal` conversion. Returns None when
/// required fields are missing (e.g. mid-rollout publisher).
fn payload_to_signal(p: MatchedPayload) -> Option<Signal> {
    let chain_id = p.chain_id?;
    let token_address = p.token_address?;
    let matched_at = p.matched_at.unwrap_or_else(Utc::now);
    Some(Signal {
        call_id: p.call_id,
        chain_id,
        token_address,
        symbol: None,
        price_usd: None,
        market_cap_usd: p.market_cap_usd.as_deref().and_then(|s| s.parse().ok()),
        liquidity_usd: p.liquidity_usd.as_deref().and_then(|s| s.parse().ok()),
        // `called_at` isn't in the matched payload (it's the original
        // call timestamp from `alpha_calls`), but `matched_at` is the
        // freshest signal-age the bot has. Use it — the bot's
        // `max_age_secs` filter is intended to skip stale matches and
        // `matched_at` is exactly that semantic.
        called_at: matched_at,
        matched_preset_id: p.preset_id,
        // `amm_address` is populated server-side for Raydium tokens
        // (factory_id "raydium" / "raydium_amm_v4"). Non-Raydium tokens
        // get `None` and the bot falls back to the PumpFun / Jupiter path.
        amm_address: p.amm_address,
    })
}

/// Open a single push stream against the gateway. Caller polls the
/// returned `mpsc::Receiver` for `Signal` events; the spawned task
/// auto-reconnects on transport failure (signals never get lost
/// because the consumer-side dedup handles re-deliveries).
///
/// ## Cancellation
///
/// Drop the receiver to stop the reconnect loop. The spawned task
/// observes the closed channel on its next yield and exits cleanly.
pub async fn spawn_subscriber(
    gateway_base: String,
    token: String,
    preset_ids: Vec<String>,
) -> Result<mpsc::Receiver<Signal>, WsStreamError> {
    if preset_ids.is_empty() {
        return Err(WsStreamError::BadUrl("no preset_ids supplied".into()));
    }
    let ws_url = build_ws_url(&gateway_base, &token)?;
    let (tx, rx) = mpsc::channel::<Signal>(64);

    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);

        loop {
            if tx.is_closed() {
                tracing::debug!("ws subscriber: receiver dropped, exiting");
                return;
            }
            match run_once(ws_url.clone(), &preset_ids, &tx).await {
                Ok(()) => {
                    // Clean exit from the inner loop — only happens
                    // when the consumer drops the receiver.
                    tracing::debug!("ws subscriber inner loop exited cleanly");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        backoff_ms = backoff.as_millis(),
                        "ws subscriber error, reconnecting"
                    );
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    });

    Ok(rx)
}

async fn run_once(
    url: String,
    preset_ids: &[String],
    tx: &mpsc::Sender<Signal>,
) -> Result<(), WsStreamError> {
    let req = url
        .as_str()
        .into_client_request()
        .map_err(|e| WsStreamError::BadUrl(e.to_string()))?;
    let (mut ws, _resp) = connect_async(req)
        .await
        .map_err(|e| WsStreamError::Connect(e.to_string()))?;

    // Subscribe frame. One frame for all preset_ids — the gateway
    // accepts a list, and that minimises the wire chatter.
    let subjects: Vec<String> = preset_ids
        .iter()
        .map(|p| format!("alpha.signals.matched.{p}"))
        .collect();
    let sub_frame = serde_json::json!({
        "type": "subscribe",
        "subjects": subjects,
    })
    .to_string();
    ws.send(Message::Text(sub_frame))
        .await
        .map_err(|e| WsStreamError::Send(e.to_string()))?;

    tracing::info!(
        n = preset_ids.len(),
        "ws subscribed to alpha.signals.matched.*"
    );

    // Periodic ping so the gateway doesn't reap us as idle. The gateway
    // sends its own pings every 30 s; ours are belt-and-suspenders.
    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                let ping = serde_json::json!({"type": "ping"}).to_string();
                ws.send(Message::Text(ping))
                    .await
                    .map_err(|e| WsStreamError::Send(e.to_string()))?;
            }
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                let msg = msg.map_err(|e| WsStreamError::Send(e.to_string()))?;
                match msg {
                    Message::Text(t) => {
                        if let Some(sig) = handle_frame(&t) {
                            if tx.send(sig).await.is_err() {
                                // Consumer dropped — exit cleanly.
                                return Ok(());
                            }
                        }
                    }
                    Message::Binary(_) => {
                        // The gateway speaks text-JSON only; binary is unexpected
                        // and we just ignore it.
                    }
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p))
                            .await
                            .map_err(|e| WsStreamError::Send(e.to_string()))?;
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        return Err(WsStreamError::Connect("server closed".into()));
                    }
                }
            }
        }
    }
    Err(WsStreamError::Connect("stream ended".into()))
}

fn handle_frame(text: &str) -> Option<Signal> {
    let frame: InboundFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(?e, "ws frame decode failed");
            return None;
        }
    };
    match frame {
        InboundFrame::Msg { subject, data } => {
            // Subject prefix sanity-check; the gateway shouldn't ship
            // us anything we didn't subscribe to, but defensive coding
            // is cheap here.
            if !subject.starts_with("alpha.signals.matched.") {
                tracing::debug!(%subject, "ignoring unexpected subject");
                return None;
            }
            let env: MatchedEnvelope = match serde_json::from_value(data) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(?e, %subject, "matched envelope decode failed");
                    return None;
                }
            };
            // Skip backfill matches — the bot must only act on live signals.
            if env.payload.backfill.unwrap_or(false) {
                return None;
            }
            payload_to_signal(env.payload)
        }
        InboundFrame::Pong => None,
        InboundFrame::Error { msg } => {
            tracing::warn!(%msg, "gateway ws error frame");
            None
        }
    }
}

fn build_ws_url(gateway_base: &str, token: &str) -> Result<String, WsStreamError> {
    // Accept either http(s)://… or ws(s)://… and rewrite scheme.
    let parsed = Url::parse(gateway_base).map_err(|e| WsStreamError::BadUrl(e.to_string()))?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(WsStreamError::BadUrl(format!(
                "unsupported scheme: {other}"
            )))
        }
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| WsStreamError::BadUrl("no host".into()))?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    Ok(format!("{scheme}://{host}{port}/ws?token={token_enc}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn build_ws_url_token_is_url_encoded() {
        // form_urlencoded uses `+` for space, `%2B` for literal +.
        // The gateway's axum query extractor handles either form.
        let u = build_ws_url("https://api.example.com", "a b+c").unwrap();
        assert!(u.contains("token=a+b%2Bc"), "got {u}");
    }

    #[test]
    fn build_ws_url_rejects_bad_scheme() {
        assert!(build_ws_url("ftp://x.example.com", "t").is_err());
    }

    #[test]
    fn payload_to_signal_happy_path() {
        let p = MatchedPayload {
            call_id: "00000000-0000-0000-0000-000000000001".into(),
            preset_id: "00000000-0000-0000-0000-000000000002".into(),
            matched_at: Some(Utc::now()),
            chain_id: Some(1),
            token_address: Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into()),
            market_cap_usd: Some("500000".into()),
            liquidity_usd: Some("80000".into()),
            backfill: None,
            amm_address: None,
        };
        let sig = payload_to_signal(p).unwrap();
        assert_eq!(sig.chain_id, 1);
        assert_eq!(
            sig.token_address,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        );
        assert_eq!(sig.market_cap_usd, Some(500_000.0));
        assert_eq!(sig.liquidity_usd, Some(80_000.0));
    }

    #[test]
    fn payload_to_signal_passes_amm_address_through() {
        let raydium_pool = "7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX";
        let p = MatchedPayload {
            call_id: "00000000-0000-0000-0000-000000000001".into(),
            preset_id: "00000000-0000-0000-0000-000000000002".into(),
            matched_at: Some(Utc::now()),
            chain_id: Some(1),
            token_address: Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into()),
            market_cap_usd: None,
            liquidity_usd: None,
            backfill: None,
            amm_address: Some(raydium_pool.into()),
        };
        let sig = payload_to_signal(p).unwrap();
        assert_eq!(sig.amm_address.as_deref(), Some(raydium_pool));
    }

    #[test]
    fn payload_to_signal_amm_address_none_for_pumpfun() {
        // PumpFun signals never have amm_address in the payload.
        let p = MatchedPayload {
            call_id: "00000000-0000-0000-0000-000000000001".into(),
            preset_id: "00000000-0000-0000-0000-000000000002".into(),
            matched_at: Some(Utc::now()),
            chain_id: Some(1),
            token_address: Some("PUMPFuNmintXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into()),
            market_cap_usd: None,
            liquidity_usd: None,
            backfill: None,
            amm_address: None,
        };
        let sig = payload_to_signal(p).unwrap();
        assert!(sig.amm_address.is_none());
    }

    #[test]
    fn handle_frame_msg_carries_amm_address() {
        let raydium_pool = "7XawhbbxtsRcQA8KTkHT9f9nc6d69UwqCDh6U5EEbEmX";
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "alpha.signals.matched.00000000-0000-0000-0000-000000000002",
            "data": {
                "payload": {
                    "call_id":       "00000000-0000-0000-0000-000000000001",
                    "preset_id":     "00000000-0000-0000-0000-000000000002",
                    "preset_version": 1,
                    "matched_at":    "2026-01-01T00:00:00Z",
                    "chain_id":      1,
                    "token_address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                    "amm_address":   raydium_pool,
                }
            }
        });
        let sig = handle_frame(&frame.to_string()).expect("yields a signal");
        assert_eq!(sig.amm_address.as_deref(), Some(raydium_pool));
    }

    #[test]
    fn payload_to_signal_skips_when_required_fields_missing() {
        // Missing token_address — bot can't trade without it, so we
        // intentionally drop the signal rather than fabricate a value.
        let p = MatchedPayload {
            call_id: "00000000-0000-0000-0000-000000000001".into(),
            preset_id: "00000000-0000-0000-0000-000000000002".into(),
            matched_at: None,
            chain_id: Some(1),
            token_address: None,
            market_cap_usd: None,
            liquidity_usd: None,
            backfill: None,
            amm_address: None,
        };
        assert!(payload_to_signal(p).is_none());
    }

    #[test]
    fn handle_frame_msg_with_backfill_is_skipped() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "alpha.signals.matched.00000000-0000-0000-0000-000000000002",
            "data": {
                "payload": {
                    "call_id":   "00000000-0000-0000-0000-000000000001",
                    "preset_id": "00000000-0000-0000-0000-000000000002",
                    "preset_version": 1,
                    "matched_at": "2026-01-01T00:00:00Z",
                    "chain_id": 1,
                    "token_address": "ATA111",
                    "backfill": true
                }
            }
        });
        assert!(handle_frame(&frame.to_string()).is_none());
    }

    #[test]
    fn handle_frame_msg_live_is_yielded() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "alpha.signals.matched.00000000-0000-0000-0000-000000000002",
            "data": {
                "payload": {
                    "call_id":   "00000000-0000-0000-0000-000000000001",
                    "preset_id": "00000000-0000-0000-0000-000000000002",
                    "preset_version": 1,
                    "matched_at": "2026-01-01T00:00:00Z",
                    "chain_id": 1,
                    "token_address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
                }
            }
        });
        let sig = handle_frame(&frame.to_string()).expect("yields a signal");
        assert_eq!(sig.chain_id, 1);
        assert_eq!(
            sig.token_address,
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        );
    }

    #[test]
    fn handle_frame_ignores_unrelated_subjects() {
        let frame = serde_json::json!({
            "type": "msg",
            "subject": "alpha.calls.update.00000000-0000-0000-0000-000000000099",
            "data": { "payload": { "kind": "tick" } }
        });
        assert!(handle_frame(&frame.to_string()).is_none());
    }

    #[test]
    fn handle_frame_error_returns_none() {
        let frame = serde_json::json!({ "type": "error", "msg": "rate limited" });
        assert!(handle_frame(&frame.to_string()).is_none());
    }
}
