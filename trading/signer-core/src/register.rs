//! Signer registration — shared paired-with-account + TOTP redemption
//! types for the HL signer and the future Solana desktop signer.
//!
//! The HTTP shape lives on the gateway in
//! `crates/modules/hyperliquid/src/exchange/api.rs` ::
//! `RedeemRegistrationBody`. We mirror it here so signer-core consumers
//! (and tests) have a stable Rust struct without dragging in the
//! gateway crate.

use serde::{Deserialize, Serialize};

/// Body the local CLI POSTs to
/// `/api/hyperliquid/exchange/signer/redeem-registration`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemRegistration {
    /// One-time `rt_…` token surfaced by the dashboard.
    pub token: String,
    /// Address derived from the local API agent secret. 0x-prefixed.
    pub agent_address: String,
    pub client_version: Option<String>,
    pub host_id: Option<String>,
    /// 6-digit TOTP code from the user's authenticator. Required when
    /// the deployment has 2FA enforcement on (default in prod).
    pub totp_code: Option<String>,
    /// HL account address (`0x…`) this signer pairs with. One signer
    /// ↔ one account; to switch the user must re-register. Recorded
    /// server-side in `hl_signer_heartbeats.paired_with_account`.
    pub paired_with_account: Option<String>,
    /// ed25519 public key (hex, 32 bytes) for the relay endpoint to
    /// verify outbound frames against.
    pub signer_pubkey_hex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemRegistrationResponse {
    pub user_id: uuid::Uuid,
    pub agent_address: String,
    pub registered_at: chrono::DateTime<chrono::Utc>,
}

/// Helper: validate the basic shape of a paired-with-account string
/// before sending the redeem POST. Catches typos on the client side
/// rather than waiting for the 400 from the server.
pub fn validate_account_format(addr: &str) -> Result<(), String> {
    if !addr.starts_with("0x") || addr.len() != 42 {
        return Err("paired_with_account must be 0x-prefixed 20-byte hex".into());
    }
    if !addr[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("paired_with_account has non-hex characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_format_validates() {
        assert!(validate_account_format("0x1234567890abcdef1234567890abcdef12345678").is_ok());
        assert!(validate_account_format("0xZZZ").is_err());
        assert!(validate_account_format("1234").is_err());
    }
}
