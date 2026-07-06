//! Gateway-WS push subscriber for HL order pickup — the GUI's nudge feed.
//!
//! ## Why this exists (L1: order pickup latency)
//!
//! The HL daemon's money path is a poll loop over `GET
//! /instructions/pending` (see [`crate::hl::daemon`]). The poll cadence
//! (`poll_secs`, default 3) bounds how long a freshly-queued order waits
//! before it's claimed and signed. [`crate::hl::daemon::DaemonHooks::nudge`]
//! is the escape hatch: a message on that channel makes the next poll fire
//! NOW instead of waiting out the ticker, so push latency stays sub-second.
//!
//! The `hl-signer-desktop` CLI feeds the nudge from a raw `async-nats`
//! subscriber on `hyperliquid.intent.exec.{user_id}`. A *desktop* client
//! behind a user's firewall can't reach the NATS broker (`:4223` is
//! server-internal) — but it CAN reach the gateway's per-user WS
//! multiplexer (`crates/platform/ws`, route `/ws`), which fans out that
//! exact subject after JWT-checking the user. The Solana side already
//! reuses that surface ([`crate::ws_stream`] / [`crate::sell_stream`]);
//! this is the HL equivalent, kept generic — it carries no payload, it
//! only translates "a message landed on my intent subject" into a nudge.
//!
//! ## Wire shapes (gateway `/ws`)
//!
//! Outbound subscribe frame:
//!   `{ "type": "subscribe", "subjects": ["hyperliquid.intent.exec.<uid>"] }`
//!
//! Inbound (per queued instruction):
//!   `{ "type": "msg", "subject": "hyperliquid.intent.exec.<uid>", "data": … }`
//!
//! We don't decode `data` — the poll loop is the source of truth for the
//! actual instruction (claim-on-read). A `msg` frame is purely a "poll
//! now" signal.
//!
//! ## Reconnect strategy
//!
//! Reconnect-forever with exponential backoff (1s → … → 30s cap), the same
//! envelope the CLI's NATS supervisor and the Solana WS streams use. Losing
//! push permanently must NOT be silent — it would degrade every order to
//! poll-interval latency — so a dropped stream always retries. The poll
//! loop remains the safety net throughout, so a missed nudge only costs one
//! poll interval, never a lost order.

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, protocol::Message},
};
use tracing::{debug, info, warn};

/// The gateway NATS subject the executor publishes to when it queues an
/// instruction for this user's signer. Pinned in one place so the
/// subscribe subject can never drift from the publisher
/// (`module-hyperliquid::exchange::executor`).
fn intent_subject(user_id: &str) -> String {
    format!("hyperliquid.intent.exec.{user_id}")
}

/// Rewrite an `http(s)`/`ws(s)` gateway origin into the `/ws?token=` URL
/// the multiplexer expects. The JWT rides in the query because WS
/// handshakes can't carry an `Authorization` header from every client.
/// Pure for unit testing.
fn build_ws_url(gateway_base: &str, token: &str) -> Result<String, String> {
    let parsed = url::Url::parse(gateway_base).map_err(|e| e.to_string())?;
    let scheme = match parsed.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => return Err(format!("unsupported scheme: {other}")),
    };
    let host = parsed.host_str().ok_or_else(|| "no host".to_string())?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    Ok(format!("{scheme}://{host}{port}/ws?token={token_enc}"))
}

/// Double the backoff, capped. Pure so the 1s→2s→…→30s schedule is
/// unit-testable (matches the CLI NATS supervisor + Solana WS streams).
fn next_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

/// Decide whether an inbound WS frame should fire a nudge. Pure: returns
/// `true` only for a `msg` frame on our intent subject. Pong / error /
/// other-subject frames never nudge. Tested against the gateway's exact
/// `ServerMsg` JSON shape.
fn frame_is_intent_nudge(text: &str, subject: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    v.get("type").and_then(|t| t.as_str()) == Some("msg")
        && v.get("subject").and_then(|s| s.as_str()) == Some(subject)
}

/// Spawn the reconnect-forever HL intent-exec push subscriber.
///
/// Connects to the gateway `/ws` with `token`, subscribes to
/// `hyperliquid.intent.exec.{user_id}`, and fires `nudge.try_send(())` on
/// every inbound `msg` frame so the daemon's poll loop wakes immediately.
/// The task lives until the nudge receiver is dropped (the daemon exits)
/// — it observes the closed channel and stops.
///
/// `try_send` is intentional: the nudge channel is a 1-deep "poll soon"
/// flag, not a queue. If a nudge is already pending we don't need a
/// second — coalescing N rapid publishes into one poll is exactly right
/// (one poll claims the whole pending batch).
pub fn spawn_intent_nudge_subscriber(
    gateway_base: String,
    token: String,
    user_id: String,
    nudge: mpsc::Sender<()>,
) {
    tokio::spawn(async move {
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        let subject = intent_subject(&user_id);
        let ws_url = match build_ws_url(&gateway_base, &token) {
            Ok(u) => u,
            Err(e) => {
                warn!(%e, "HL push: bad gateway url — order pickup falls back to poll-only");
                return;
            }
        };
        let mut backoff = Duration::from_secs(1);
        loop {
            if nudge.is_closed() {
                debug!("HL push: nudge receiver dropped — stopping subscriber");
                return;
            }
            match run_once(&ws_url, &subject, &nudge).await {
                Ok(()) => {
                    // Inner loop only returns Ok when the nudge channel
                    // closed (daemon exited) — stop cleanly.
                    debug!("HL push: subscriber inner loop exited cleanly");
                    return;
                }
                Err(e) => {
                    warn!(
                        %e,
                        backoff_secs = backoff.as_secs(),
                        "HL push: stream error — reconnecting (poll loop still covers pickup)"
                    );
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = next_backoff(backoff, MAX_BACKOFF);
        }
    });
}

/// One connect→subscribe→pump cycle. Returns `Ok(())` only when the nudge
/// channel closed (caller stops); any transport/protocol failure is an
/// `Err` the caller backs off and retries.
async fn run_once(ws_url: &str, subject: &str, nudge: &mpsc::Sender<()>) -> Result<(), String> {
    let req = ws_url
        .into_client_request()
        .map_err(|e| format!("request: {e}"))?;
    let (mut ws, _resp) = connect_async(req)
        .await
        .map_err(|e| format!("connect: {e}"))?;

    let sub_frame = serde_json::json!({
        "type": "subscribe",
        "subjects": [subject],
    })
    .to_string();
    ws.send(Message::Text(sub_frame))
        .await
        .map_err(|e| format!("subscribe send: {e}"))?;
    info!(%subject, "HL push: subscribed for sub-second order pickup");

    // Our own keepalive ping; the gateway also pings us every 30s.
    let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if nudge.is_closed() {
            return Ok(());
        }
        tokio::select! {
            _ = ping_tick.tick() => {
                let ping = serde_json::json!({"type": "ping"}).to_string();
                ws.send(Message::Text(ping))
                    .await
                    .map_err(|e| format!("ping send: {e}"))?;
            }
            msg = ws.next() => {
                let Some(msg) = msg else {
                    return Err("stream ended".into());
                };
                let msg = msg.map_err(|e| format!("recv: {e}"))?;
                match msg {
                    Message::Text(t) => {
                        if frame_is_intent_nudge(&t, subject) {
                            debug!("HL push: intent landed — nudging poll loop");
                            // Coalescing send: a full channel already has a
                            // pending nudge, which is all we need.
                            let _ = nudge.try_send(());
                        }
                    }
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p))
                            .await
                            .map_err(|e| format!("pong send: {e}"))?;
                    }
                    Message::Close(_) => return Err("server closed".into()),
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_matches_executor_publish_format() {
        // Must equal `module-hyperliquid::exchange::executor`'s publish
        // subject byte-for-byte — a drift here silently kills push pickup.
        assert_eq!(
            intent_subject("1234abcd"),
            "hyperliquid.intent.exec.1234abcd"
        );
    }

    #[test]
    fn build_ws_url_https_to_wss_on_ws_route() {
        let u = build_ws_url("https://api-v2.degenbox.app", "tok").unwrap();
        assert_eq!(u, "wss://api-v2.degenbox.app/ws?token=tok");
    }

    #[test]
    fn build_ws_url_http_to_ws_keeps_port() {
        let u = build_ws_url("http://localhost:8090", "tok").unwrap();
        assert_eq!(u, "ws://localhost:8090/ws?token=tok");
    }

    #[test]
    fn build_ws_url_url_encodes_jwt() {
        // A real JWT has `.` (safe) but the encoder must handle `+` / `=`
        // in base64url-with-padding edge cases without corrupting the token.
        let u = build_ws_url("https://x.example", "a b+c=d").unwrap();
        assert!(u.contains("token=a+b%2Bc%3Dd"), "got {u}");
    }

    #[test]
    fn build_ws_url_rejects_bad_scheme() {
        assert!(build_ws_url("ftp://x", "t").is_err());
    }

    #[test]
    fn backoff_doubles_then_caps_at_30s() {
        let max = Duration::from_secs(30);
        let mut b = Duration::from_secs(1);
        let mut seq = vec![b.as_secs()];
        for _ in 0..6 {
            b = next_backoff(b, max);
            seq.push(b.as_secs());
        }
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn only_msg_frame_on_our_subject_nudges() {
        let subject = "hyperliquid.intent.exec.uid1";
        // The real gateway `ServerMsg::Msg` shape.
        let msg = serde_json::json!({
            "type": "msg",
            "subject": subject,
            "data": { "payload": { "cloid": "c1" } },
        })
        .to_string();
        assert!(frame_is_intent_nudge(&msg, subject));
    }

    #[test]
    fn pong_and_error_frames_do_not_nudge() {
        let subject = "hyperliquid.intent.exec.uid1";
        assert!(!frame_is_intent_nudge(
            &serde_json::json!({"type": "pong"}).to_string(),
            subject
        ));
        assert!(!frame_is_intent_nudge(
            &serde_json::json!({"type": "error", "msg": "rate limited"}).to_string(),
            subject
        ));
    }

    #[test]
    fn msg_for_a_different_user_subject_does_not_nudge() {
        // Defensive: the gateway is user-scoped so this shouldn't arrive,
        // but a stray frame for someone else's subject must never poke us.
        let mine = "hyperliquid.intent.exec.uid1";
        let other = serde_json::json!({
            "type": "msg",
            "subject": "hyperliquid.intent.exec.uid2",
            "data": {},
        })
        .to_string();
        assert!(!frame_is_intent_nudge(&other, mine));
    }

    #[test]
    fn malformed_frame_does_not_nudge() {
        assert!(!frame_is_intent_nudge(
            "not json",
            "hyperliquid.intent.exec.uid1"
        ));
        assert!(!frame_is_intent_nudge("{}", "hyperliquid.intent.exec.uid1"));
    }
}
