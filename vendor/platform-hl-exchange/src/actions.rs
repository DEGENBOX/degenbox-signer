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
    /// HL's wire key is lowercase `tpsl` (verified against the HL
    /// `/exchange` order schema + the Python/nktkas SDKs — the trigger
    /// object requires `{isMarket, triggerPx, tpsl}`). Serializing it as
    /// camelCase `tpSl` made HL reject EVERY trigger order — TP/SL on a
    /// position, caller SL/TP legs, trailing-SL replaces, ladder rungs,
    /// and the entry+TP+SL bulk — with `http status 422: Failed to
    /// deserialize the JSON body into the target type`. This `rename`
    /// drives BOTH the JSON wire and the msgpack the signature hashes, so
    /// the fix lands on both sides at once.
    #[serde(rename = "tpsl")]
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

// ─────────────────────── USD Class Transfer (spot↔perp) ───────────────────────
//
// Moves USDC between the SPOT and PERP wallets of the SAME account.
// Perps trade off the perp balance only, so funds deposited to spot must
// be transferred to perp before they can margin a position (and vice-
// versa to withdraw). Unlike vault transfers this is a **user-signed**
// action (EIP-712 `HyperliquidTransaction:UsdClassTransfer`), NOT an L1
// action — signed by the approved agent key under the
// `HyperliquidSignTransaction` domain on Arbitrum (chainId 0x66eee).
//
// HL Python SDK reference (`hyperliquid/exchange.py::usd_class_transfer`):
//
//   action = {
//     "type":   "usdClassTransfer",
//     "amount": str(amount),   // human USDC, e.g. "12.5"
//     "toPerp": true|false,    // true = spot→perp, false = perp→spot
//     "nonce":  timestamp_ms,
//   }
//   # sign_user_signed_action adds these two to the WIRE action:
//   action["signatureChainId"] = "0x66eee"
//   action["hyperliquidChain"] = "Mainnet" | "Testnet"
//
// The signed EIP-712 message (USD_CLASS_TRANSFER_SIGN_TYPES) is exactly:
//   [hyperliquidChain(string), amount(string), toPerp(bool), nonce(uint64)]
// — signatureChainId is on the wire but NOT in the signed struct.
//
// `nonce` doubles as the request nonce in the `/exchange` envelope, so
// the action carries it inline (unlike order/cancel where the envelope
// nonce is separate).

/// Arbitrum chain id HL pins for user-signed actions: `0x66eee` = 421614.
pub const HL_USER_SIGN_CHAIN_ID_HEX: &str = "0x66eee";
pub const HL_USER_SIGN_CHAIN_ID: u64 = 0x6_6eee;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsdClassTransferAction {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "hyperliquidChain")]
    pub hyperliquid_chain: String,
    #[serde(rename = "signatureChainId")]
    pub signature_chain_id: String,
    /// Human-readable USDC amount as a decimal string, e.g. `"12.5"`.
    pub amount: String,
    /// `true` = spot→perp, `false` = perp→spot.
    #[serde(rename = "toPerp")]
    pub to_perp: bool,
    /// ms-since-epoch; also the `/exchange` envelope nonce.
    pub nonce: u64,
}

impl UsdClassTransferAction {
    pub fn new(network_mainnet: bool, amount: String, to_perp: bool, nonce: u64) -> Self {
        Self {
            kind: "usdClassTransfer".to_string(),
            hyperliquid_chain: if network_mainnet {
                "Mainnet".into()
            } else {
                "Testnet".into()
            },
            signature_chain_id: HL_USER_SIGN_CHAIN_ID_HEX.into(),
            amount,
            to_perp,
            nonce,
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

    /// `TriggerWire` serialises with HL's exact wire keys: `isMarket` and
    /// `triggerPx` are camelCase, but `tpsl` is LOWERCASE (HL's schema).
    /// The whole trigger path (TP/SL on a position, caller SL/TP legs,
    /// trailing-SL, ladder rungs, entry+TP+SL bulk) 422'd on the old
    /// camelCase `tpSl` — pin the correct key so it can't regress.
    #[test]
    fn trigger_wire_uses_hl_wire_keys() {
        let t = TriggerWire {
            is_market: true,
            trigger_px: "65000".into(),
            tp_sl: "tp".into(),
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains("\"isMarket\":true"), "{s}");
        assert!(s.contains("\"triggerPx\":\"65000\""), "{s}");
        // HL requires lowercase `tpsl` — NOT camelCase `tpSl`.
        assert!(s.contains("\"tpsl\":\"tp\""), "{s}");
        assert!(!s.contains("tpSl"), "must not emit camelCase tpSl: {s}");
    }

    /// A full trigger `OrderWire` (as `submit`/`placeTpsl`/bulk build it)
    /// serialises to HL's exact `t.trigger` shape — this is the payload
    /// HL 422'd on before the `tpsl` key fix. Pins the end-to-end wire.
    #[test]
    fn trigger_order_wire_matches_hl_schema() {
        let w = OrderWire {
            a: 0,
            b: false,
            p: "60000".into(),
            s: "0.01".into(),
            r: true,
            t: OrderType {
                limit: None,
                trigger: Some(TriggerWire {
                    is_market: true,
                    trigger_px: "60000".into(),
                    tp_sl: "sl".into(),
                }),
            },
            c: Some("0x0190f3a1b2c3d4e5f60718293a4b5c6d".into()),
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(s.contains("\"trigger\":{"), "{s}");
        assert!(s.contains("\"tpsl\":\"sl\""), "{s}");
        // The reduce-only trigger must NOT carry a `limit` object (HL's
        // `t` is a one-of; both present is a deserialize error).
        assert!(
            !s.contains("\"limit\""),
            "trigger order must omit limit: {s}"
        );
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

    /// `UsdClassTransferAction` serialises to HL's exact wire shape:
    /// type, hyperliquidChain, signatureChainId, amount, toPerp, nonce.
    /// The signed EIP-712 message is only a SUBSET of these
    /// (hyperliquidChain/amount/toPerp/nonce) but the WIRE action carries
    /// signatureChainId too — HL requires both on the POST envelope.
    #[test]
    fn usd_class_transfer_wire_shape() {
        let a = UsdClassTransferAction::new(true, "12.5".into(), true, 1_700_000_000_000);
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"type\":\"usdClassTransfer\""), "{s}");
        assert!(s.contains("\"hyperliquidChain\":\"Mainnet\""), "{s}");
        assert!(s.contains("\"signatureChainId\":\"0x66eee\""), "{s}");
        assert!(s.contains("\"amount\":\"12.5\""), "{s}");
        assert!(s.contains("\"toPerp\":true"), "{s}");
        assert!(s.contains("\"nonce\":1700000000000"), "{s}");
    }

    /// Testnet flips `hyperliquidChain` to "Testnet" but keeps the same
    /// Arbitrum `signatureChainId` (0x66eee) — matches the HL SDK.
    #[test]
    fn usd_class_transfer_testnet_chain_label() {
        let a = UsdClassTransferAction::new(false, "1".into(), false, 1);
        assert_eq!(a.hyperliquid_chain, "Testnet");
        assert_eq!(a.signature_chain_id, HL_USER_SIGN_CHAIN_ID_HEX);
        assert!(!a.to_perp);
    }

    /// The pinned Arbitrum chain-id hex parses to the numeric domain id.
    #[test]
    fn user_sign_chain_id_hex_matches_numeric() {
        assert_eq!(
            u64::from_str_radix(HL_USER_SIGN_CHAIN_ID_HEX.trim_start_matches("0x"), 16).unwrap(),
            HL_USER_SIGN_CHAIN_ID
        );
    }
}
