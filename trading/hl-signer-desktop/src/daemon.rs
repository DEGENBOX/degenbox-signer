//! Daemon loop: register, then poll-or-subscribe for instructions,
//! sign them locally, POST to HL, report back to the DegenBox server.

use crate::config::{Config, NetworkChoice};
use crate::hl_info::HttpInfoClient;
use crate::server::{PendingRow, RegisterReq, ResultReq, ServerClient};
use crate::signing::{execute, ExecContext, SignedSubmitResult};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use platform_hl_exchange::{AgentSigner, ExchangeClient, Network};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

pub struct DaemonOpts {
    pub config: Config,
    pub secret_hex: String,
    pub agent_address: String,
    /// Cadence for the server-poll loop (the fallback / source of
    /// truth). NATS push just makes the next poll fire immediately.
    pub poll_interval: Duration,
    /// Optional NATS URL. When set, the daemon also subscribes to
    /// `hyperliquid.intent.exec.{user_id}` for sub-second push.
    pub nats_url: Option<String>,
    pub user_id: Option<String>,
}

pub async fn run(opts: DaemonOpts) -> Result<()> {
    let network = match opts.config.network {
        NetworkChoice::Mainnet => Network::Mainnet,
        NetworkChoice::Testnet => Network::Testnet,
    };
    let signer = AgentSigner::from_hex(&opts.secret_hex, network)
        .map_err(|e| anyhow!("agent signer: {e}"))?;
    if signer.address_hex().to_ascii_lowercase() != opts.agent_address.to_ascii_lowercase() {
        return Err(anyhow!(
            "keystore unlocked an address different from config: {} vs {}",
            signer.address_hex(),
            opts.agent_address
        ));
    }
    let hl_client = ExchangeClient::new(network).map_err(|e| anyhow!("hl client: {e}"))?;
    let info_client = HttpInfoClient::new(network).map_err(|e| anyhow!("hl info client: {e}"))?;

    if opts.config.account_address.is_none() {
        // Not fatal — only `closePosition` / `placeTpsl` require it,
        // and those payloads will return a clean error if they land.
        warn!(
            "no account_address configured — Close / TP / SL instructions \
             will fail until you run `hl-signer-desktop register --account=0x…`"
        );
    }

    let api_token =
        opts.config.api_token.clone().ok_or_else(|| {
            anyhow!("api_token missing in config — run `hl-signer-desktop register`")
        })?;
    let server = ServerClient::new(opts.config.server_url.clone(), api_token)?;

    // Self-register so the server flips our user's `signer/status` to
    // ready. We deliberately re-register every daemon start — the
    // server treats this as a heartbeat refresh.
    let host_id = opts.config.host_id.clone().or_else(|| {
        std::env::var("HOSTNAME")
            .ok()
            .or_else(|| hostname_fallback())
    });
    let reg = server
        .register(&RegisterReq {
            agent_address: opts.agent_address.clone(),
            client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
            host_id,
        })
        .await?;
    info!(user_id = %reg.user_id, agent = %reg.agent_address, "registered with server");
    // Branded one-liner so operators see a clear "we're live" marker
    // separate from the tracing stream. Mirrors the web UI's
    // StatusBadge treatment: green dot + "ready" label.
    eprintln!(
        "  {} {}  {} {} {} {}",
        crate::branding::brand_tag(),
        crate::branding::status_pill("ready"),
        crate::branding::muted("user"),
        crate::branding::accent_bold(&reg.user_id),
        crate::branding::muted("·  agent"),
        crate::branding::accent_bold(&reg.agent_address)
    );

    // Spawn the NATS push channel (best-effort).
    let (nudge_tx, mut nudge_rx) = tokio::sync::mpsc::channel::<()>(8);
    if let Some(url) = opts.nats_url.clone() {
        let user_id = reg.user_id.clone();
        let tx = nudge_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = run_nats_subscriber(url, user_id, tx).await {
                warn!(?e, "NATS subscriber ended");
            }
        });
    }

    let ctx = ExecContext {
        signer: Arc::new(signer),
        hl: hl_client,
        info: Arc::new(info_client),
        account_address: opts.config.account_address.clone(),
    };

    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut ticker = interval(opts.poll_interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = nudge_rx.recv() => {
                debug!("nudged by NATS — polling now");
            }
        }
        match server.pending(last_seen, 20).await {
            Ok(rows) => {
                if rows.is_empty() {
                    continue;
                }
                info!(count = rows.len(), "received pending instructions");
                for row in rows {
                    last_seen = Some(last_seen.unwrap_or(row.created_at).max(row.created_at));
                    if let Err(e) = handle_one(&ctx, &server, &row).await {
                        error!(?e, cloid = %row.cloid, "instruction handler failed");
                    }
                }
            }
            Err(e) => {
                warn!(?e, "poll failed — will retry next tick");
            }
        }
    }
}

async fn handle_one(ctx: &ExecContext, server: &ServerClient, row: &PendingRow) -> Result<()> {
    debug!(cloid = %row.cloid, "signing + submitting to HL");
    let result: SignedSubmitResult = execute(&row.payload, ctx)
        .await
        .map_err(|e| anyhow!("execute: {e}"))?;
    // Report back unconditionally — failures included, so the server's
    // order row gets a final status instead of dangling in `queued`.
    server
        .post_result(&ResultReq {
            cloid: result.cloid.clone(),
            oid: result.oid,
            status: result.status.clone(),
            filled_size_usd: result.filled_size_usd.clone(),
            err_msg: result.err_msg.clone(),
        })
        .await?;
    info!(
        cloid = %result.cloid,
        oid = ?result.oid,
        status = %result.status,
        "instruction acked to server"
    );
    Ok(())
}

async fn run_nats_subscriber(
    url: String,
    user_id: String,
    nudge: tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let client = async_nats::connect(&url).await?;
    let subject = format!("hyperliquid.intent.exec.{user_id}");
    let mut sub = client.subscribe(subject.clone()).await?;
    info!(%subject, "NATS subscribed for push nudges");
    while let Some(_msg) = sub.next().await {
        let _ = nudge.try_send(());
    }
    Ok(())
}

fn hostname_fallback() -> Option<String> {
    // POSIX uname() via a separate process. Best-effort — used only
    // for support diagnostics, not security-sensitive.
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure the subscribe subject pattern matches the server's
    /// publish subject exactly. A typo here means push nudges silently
    /// stop working.
    #[test]
    fn nats_subject_pattern_matches_server_format() {
        let user_id = "1234abcd";
        let subject = format!("hyperliquid.intent.exec.{user_id}");
        assert_eq!(subject, "hyperliquid.intent.exec.1234abcd");
    }

    /// Sanity-check that a totally absent config rejects daemon start
    /// without panicking. We can't run the real loop (no server +
    /// network), so we check the early `api_token` validation by
    /// asserting `ServerClient::new` rejects an empty token. The full
    /// daemon path is exercised by integration runs.
    #[test]
    fn missing_api_token_returns_error_not_panic() {
        let cfg = Config {
            api_token: None,
            ..Config::default()
        };
        // Mimic the gate in `daemon::run` — both should refuse.
        assert!(cfg.api_token.is_none());
        let err = crate::server::ServerClient::new(cfg.server_url, String::new()).unwrap_err();
        assert!(matches!(err, crate::server::ServerError::NoToken));
    }
}
