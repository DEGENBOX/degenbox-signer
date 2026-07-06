//! HL `userFills` parsing + per-`oid` realised-PnL aggregation.
//!
//! HL's `/exchange` `order` action response does NOT carry `closedPnl`
//! — that value only lives in the `userFills` / `userEvents` stream
//! (WS) or the `userFills` / `userFillsByTime` `/info` REST endpoints.
//! The desktop signer needs it so a *close / reduce* fill can report
//! realised PnL back to the gateway, which feeds the server-side
//! circuit-breaker loss counter.
//!
//! This module is intentionally **transport-free**: it only defines the
//! `UserFill` shape (the subset of fields HL ships per fill) and pure
//! helpers to aggregate `closedPnl` for a given order id. The actual
//! `/info` POST lives in the caller (the signer's `HttpInfoClient`),
//! mirroring the existing `clearinghouseState` request pattern there —
//! `userFills` is an UNSIGNED `/info` POST keyed by the user's address
//! (`{"type":"userFills","user":"0x…"}`), exactly like every other
//! `/info` query. No agent signature is involved.
//!
//! Additive-only: introduces a new struct + free functions; touches no
//! existing exchange-action type that the `module-hyperliquid` layer
//! pattern-matches on.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

/// One entry from the HL `userFills` array.
///
/// HL ships every numeric as a JSON string. Only the fields the signer
/// needs are modelled; unknown fields are ignored by serde so HL adding
/// columns never breaks the parse.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UserFill {
    /// Order id the fill belongs to. The signer correlates by this.
    pub oid: u64,
    /// Realised PnL on this fill, string-encoded decimal. Zero (or
    /// "0.0") for the opening side of a trade; non-zero only when the
    /// fill closed/reduced an existing position.
    #[serde(rename = "closedPnl", default)]
    pub closed_pnl: Option<String>,
    /// Fill size (base units), string-encoded. Diagnostic.
    #[serde(default)]
    pub sz: Option<String>,
    /// Fill price, string-encoded. Diagnostic.
    #[serde(default)]
    pub px: Option<String>,
    /// Fee paid on the fill, string-encoded. Diagnostic — NOT folded
    /// into `closedPnl` (HL's `closedPnl` is already net of the close
    /// fee on the reducing leg).
    #[serde(default)]
    pub fee: Option<String>,
    /// Direction string (e.g. "Close Long", "Open Short"). Diagnostic.
    #[serde(default)]
    pub dir: Option<String>,
    /// Fill timestamp (ms since epoch). Diagnostic / ordering.
    #[serde(default)]
    pub time: Option<u64>,
}

/// Parse a raw `userFills` JSON array (the body HL returns from
/// `{"type":"userFills","user":"0x…"}`) into typed rows.
///
/// HL returns a bare JSON array at the top level for `userFills`.
pub fn parse_user_fills(raw: &[u8]) -> Result<Vec<UserFill>, serde_json::Error> {
    serde_json::from_slice(raw)
}

/// Sum `closedPnl` across all fills matching `oid`.
///
/// Returns:
/// - `Some(total)` when at least one matching fill carries a parseable
///   `closedPnl` — including `Some(0)` when the matched fills are all
///   genuinely zero-PnL (an opening fill). The caller decides whether a
///   zero is worth reporting.
/// - `None` when NO fill matches `oid` (not indexed yet / wrong oid).
///   The caller must treat this as "unknown" and report `closed_pnl:
///   None` rather than fabricating a zero.
///
/// A fill whose `closedPnl` is absent or unparseable contributes `0` to
/// the sum but still counts as a match (so a partial-fill set where one
/// leg lacks the field doesn't collapse the whole lookup to `None`).
pub fn sum_closed_pnl_for_oid(fills: &[UserFill], oid: u64) -> Option<Decimal> {
    let mut matched = false;
    let mut total = Decimal::ZERO;
    for f in fills {
        if f.oid != oid {
            continue;
        }
        matched = true;
        if let Some(raw) = &f.closed_pnl {
            if let Ok(d) = Decimal::from_str(raw) {
                total += d;
            }
        }
    }
    matched.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fills_json() -> &'static str {
        // Shape mirrors a real HL `userFills` response (bare array).
        r#"[
            {"oid": 100, "closedPnl": "12.5", "sz": "0.5", "px": "60000", "fee": "0.3", "dir": "Close Long", "time": 1700000000000},
            {"oid": 100, "closedPnl": "3.0",  "sz": "0.1", "px": "60100", "fee": "0.1", "dir": "Close Long", "time": 1700000000001},
            {"oid": 200, "closedPnl": "0.0",  "sz": "0.2", "px": "59000", "fee": "0.1", "dir": "Open Long",  "time": 1700000000002}
        ]"#
    }

    #[test]
    fn parse_user_fills_decodes_array() {
        let fills = parse_user_fills(fills_json().as_bytes()).unwrap();
        assert_eq!(fills.len(), 3);
        assert_eq!(fills[0].oid, 100);
        assert_eq!(fills[0].closed_pnl.as_deref(), Some("12.5"));
        assert_eq!(fills[0].dir.as_deref(), Some("Close Long"));
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        // HL may add columns (`hash`, `crossed`, `startPosition`, …).
        let raw = r#"[{"oid": 1, "closedPnl": "1.0", "hash": "0xabc", "crossed": true, "startPosition": "0.0"}]"#;
        let fills = parse_user_fills(raw.as_bytes()).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].closed_pnl.as_deref(), Some("1.0"));
    }

    #[test]
    fn sum_closed_pnl_aggregates_partial_fills() {
        // oid 100 filled in two partials: 12.5 + 3.0 = 15.5.
        let fills = parse_user_fills(fills_json().as_bytes()).unwrap();
        assert_eq!(sum_closed_pnl_for_oid(&fills, 100), Some(dec!(15.5)));
    }

    #[test]
    fn sum_closed_pnl_zero_for_opening_fill() {
        // oid 200 is an open — closedPnl is 0, but it IS matched, so we
        // return Some(0), not None.
        let fills = parse_user_fills(fills_json().as_bytes()).unwrap();
        assert_eq!(sum_closed_pnl_for_oid(&fills, 200), Some(Decimal::ZERO));
    }

    #[test]
    fn sum_closed_pnl_none_when_oid_absent() {
        // oid not in the set → unknown, caller must NOT fabricate a 0.
        let fills = parse_user_fills(fills_json().as_bytes()).unwrap();
        assert_eq!(sum_closed_pnl_for_oid(&fills, 999), None);
    }

    #[test]
    fn sum_closed_pnl_missing_field_counts_as_match_zero() {
        let raw = r#"[{"oid": 7}]"#;
        let fills = parse_user_fills(raw.as_bytes()).unwrap();
        // Matched but no closedPnl → contributes 0, still Some(0).
        assert_eq!(sum_closed_pnl_for_oid(&fills, 7), Some(Decimal::ZERO));
    }

    #[test]
    fn sum_closed_pnl_handles_negative_loss() {
        let raw = r#"[{"oid": 5, "closedPnl": "-42.75"}]"#;
        let fills = parse_user_fills(raw.as_bytes()).unwrap();
        assert_eq!(sum_closed_pnl_for_oid(&fills, 5), Some(dec!(-42.75)));
    }
}
