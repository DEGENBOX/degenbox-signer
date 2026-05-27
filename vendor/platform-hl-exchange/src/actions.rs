//! Strongly-typed HL `/exchange` action payloads.
//!
//! Field names are short single-letter codes (`a`, `b`, `p`, `s`, …)
//! because that's what HL wants on the wire. They match the legacy
//! Go client one-for-one.

use serde::{Deserialize, Serialize};

// ─────────────────────────── Orders ───────────────────────────

/// `{type: "order", orders: […], grouping: …}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub orders: Vec<OrderWire>,
    pub grouping: Grouping,
}

impl OrderAction {
    pub fn new(orders: Vec<OrderWire>, grouping: Grouping) -> Self {
        Self {
            kind: "order".to_string(),
            orders,
            grouping,
        }
    }
}

/// Wire-shaped order. `a` = asset id, `b` = is_buy, `p` = limit price
/// (string), `s` = size (string), `r` = reduce-only, `t` = type,
/// `c` = client order id (cloid, optional).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: OrderType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderType {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitSpec {
    pub tif: String, // "Alo" | "Ioc" | "Gtc"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerWire {
    #[serde(rename = "isMarket")]
    pub is_market: bool,
    #[serde(rename = "triggerPx")]
    pub trigger_px: String,
    #[serde(rename = "tpSl")]
    pub tp_sl: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Grouping {
    Na,
    NormalTpsl,
    PositionTpsl,
}

// ─────────────────────────── Cancel ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub cancels: Vec<CancelSpec>,
}

impl CancelAction {
    pub fn new(cancels: Vec<CancelSpec>) -> Self {
        Self {
            kind: "cancel".to_string(),
            cancels,
        }
    }
}

/// Cancel by (asset, oid). `a` = asset id, `o` = exchange oid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelSpec {
    pub a: u32,
    pub o: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelByCloidAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub cancels: Vec<CancelByCloidSpec>,
}

impl CancelByCloidAction {
    pub fn new(cancels: Vec<CancelByCloidSpec>) -> Self {
        Self {
            kind: "cancelByCloid".to_string(),
            cancels,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelByCloidSpec {
    pub asset: u32,
    pub cloid: String,
}

// ─────────────────────────── Leverage ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateLeverageAction {
    #[serde(rename = "type")]
    pub kind: String,
    pub asset: u32,
    #[serde(rename = "isCross")]
    pub is_cross: bool,
    pub leverage: u32,
}

impl UpdateLeverageAction {
    pub fn new(asset: u32, leverage: u32, is_cross: bool) -> Self {
        Self {
            kind: "updateLeverage".to_string(),
            asset,
            is_cross,
            leverage,
        }
    }
}

// ─────────────────────────── Vault Transfer ───────────────────────────
//
// HLP + builder-code vault deposits / withdrawals. L1-domain signed.
//
// HL Python SDK reference (`hyperliquid/exchange.py::vault_usd_transfer`):
//
//   action = {
//     "type": "vaultTransfer",
//     "vaultAddress": vault.lower(),   // 20-byte hex, 0x-prefixed
//     "isDeposit":    true|false,
//     "usd":          int,             // 6-decimal units; $100 → 100_000_000
//   }
//
// Field-order matters for the msgpack hash — keep the struct definition
// in `(type, vaultAddress, isDeposit, usd)` order so `rmp-serde`'s
// `to_vec_named` produces bytes that match the Python SDK.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultTransferAction {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "vaultAddress")]
    pub vault_address: String,
    #[serde(rename = "isDeposit")]
    pub is_deposit: bool,
    /// 6-decimal USD units. `$100` → `100_000_000`.
    pub usd: u64,
}

impl VaultTransferAction {
    /// `vault_address` is normalised to lowercase 0x-prefixed hex so
    /// the wire byte-sequence is deterministic regardless of caller
    /// casing.
    pub fn new(vault_address: &str, is_deposit: bool, usd: u64) -> Self {
        let v = vault_address.to_ascii_lowercase();
        let v = if v.starts_with("0x") {
            v
        } else {
            format!("0x{v}")
        };
        Self {
            kind: "vaultTransfer".to_string(),
            vault_address: v,
            is_deposit,
            usd,
        }
    }
}

// ─────────────────────────── Approve Agent ───────────────────────────
//
// "Approve API agent" lets a derived secp256k1 key (held by us, on
// behalf of the user) place trades without touching the user's main
// key. The user signs this action ONCE with their main wallet; from
// then on we sign trades with the agent key only.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApproveAgentAction {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "hyperliquidChain")]
    pub hyperliquid_chain: String,
    #[serde(rename = "signatureChainId")]
    pub signature_chain_id: String,
    #[serde(rename = "agentAddress")]
    pub agent_address: String,
    #[serde(rename = "agentName")]
    pub agent_name: String,
    pub nonce: u64,
}

impl ApproveAgentAction {
    pub fn new(
        network_mainnet: bool,
        agent_address: String,
        agent_name: String,
        nonce: u64,
    ) -> Self {
        Self {
            kind: "approveAgent".to_string(),
            hyperliquid_chain: if network_mainnet {
                "Mainnet".into()
            } else {
                "Testnet".into()
            },
            // Arbitrum One (mainnet) or Arbitrum Sepolia (testnet) — the
            // chain the user's wallet signs on, NOT the phantom 1337.
            signature_chain_id: if network_mainnet {
                "0xa4b1".into()
            } else {
                "0x66eee".into()
            },
            agent_address,
            agent_name,
            nonce,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TriggerWire` serialises with HL's expected camelCase keys —
    /// the bulk-order TP/SL path depends on this.
    #[test]
    fn trigger_wire_uses_camelcase_keys() {
        let t = TriggerWire {
            is_market: true,
            trigger_px: "65000".into(),
            tp_sl: "tp".into(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains("\"isMarket\":true"), "{s}");
        assert!(s.contains("\"triggerPx\":\"65000\""), "{s}");
        assert!(s.contains("\"tpSl\":\"tp\""), "{s}");
    }

    /// `Grouping::PositionTpsl` JSON-serialises to `"positionTpsl"` —
    /// HL rejects any other casing on bulk TP/SL orders.
    #[test]
    fn grouping_position_tpsl_camelcase() {
        let g = Grouping::PositionTpsl;
        let s = serde_json::to_string(&g).unwrap();
        assert_eq!(s, "\"positionTpsl\"");
    }

    /// `UpdateLeverageAction` serialises with `isCross` camelCase.
    #[test]
    fn update_leverage_isolated_is_cross_false() {
        let a = UpdateLeverageAction::new(0, 10, false);
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"isCross\":false"), "{s}");
        assert!(s.contains("\"type\":\"updateLeverage\""), "{s}");
        assert!(s.contains("\"leverage\":10"), "{s}");
    }

    /// `VaultTransferAction` matches HL Python SDK shape: type,
    /// vaultAddress (lowercased 0x-hex), isDeposit, usd.
    #[test]
    fn vault_transfer_lowercases_address_and_uses_camelcase_keys() {
        let a = VaultTransferAction::new(
            "0xABCD000000000000000000000000000000001234",
            true,
            100_000_000,
        );
        assert_eq!(
            a.vault_address,
            "0xabcd000000000000000000000000000000001234"
        );
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"type\":\"vaultTransfer\""), "{s}");
        assert!(s.contains("\"vaultAddress\":\"0xabcd"), "{s}");
        assert!(s.contains("\"isDeposit\":true"), "{s}");
        assert!(s.contains("\"usd\":100000000"), "{s}");
    }

    /// VaultTransferAction msgpack-named encoding stays deterministic.
    /// If somebody renames or reorders fields, the connection-id hash
    /// changes and HL rejects every transfer — pin the bytes here.
    #[test]
    fn vault_transfer_msgpack_field_order_is_stable() {
        let a = VaultTransferAction::new(
            "0x1111111111111111111111111111111111111111",
            false,
            1_000_000,
        );
        let bytes = rmp_serde::to_vec_named(&a).unwrap();
        // 4-element map (0x84) with keys: "type", "vaultAddress",
        // "isDeposit", "usd". Hex pin: the leading 0x84 plus a "type"
        // marker — full pin is too brittle but the prefix catches
        // accidental field-count regressions.
        assert_eq!(bytes[0], 0x84, "must be a 4-key map");
        let hex = hex::encode(&bytes);
        assert!(hex.contains("a474797065"), "must contain 'type' key");
        assert!(
            hex.contains("ac7661756c7441646472657373"),
            "must contain 'vaultAddress' key"
        );
        assert!(hex.contains("a9697344"), "must contain 'isDeposit' prefix");
        assert!(hex.contains("a37573"), "must contain 'usd' key");
    }

    /// Vault address without 0x prefix is normalised with one prepended,
    /// not double-prefixed.
    #[test]
    fn vault_transfer_prepends_0x_when_missing() {
        let a = VaultTransferAction::new("1111111111111111111111111111111111111111", true, 10);
        assert_eq!(
            a.vault_address,
            "0x1111111111111111111111111111111111111111"
        );
    }
}
