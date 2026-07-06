//! Budget guard — hard caps per-session / per-token / per-hour.
//!
//! Used by `bot-engine` (slot T.3) to enforce that a signal-driven
//! auto-trader never spends more than the user authorized. The data
//! structures + check functions live here so they're testable in
//! isolation; integration with the actual bot loop lands in T.3.
//!
//! ## Three layers
//!
//! - **Session budget** — total lamports allowed across the bot
//!   session. Reset when the user enables a fresh session.
//! - **Per-token cap** — max lamports per single token address.
//!   Defends against a "rug-pull" preset that fires repeatedly on
//!   the same token.
//! - **Per-hour cap** — sliding-window rate limit; sums spend within
//!   the trailing 60 minutes. Defends against runaway-bot scenarios
//!   where a misconfigured preset fires every second.
//!
//! All three checks must pass for the bot to issue a sign. Any
//! single failure rejects, with a structured reason the UI can show
//! to the user ("session 80% spent, please raise budget" etc).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("session budget exhausted: spent {spent_lamports} of {budget_lamports} cap")]
    SessionExhausted {
        spent_lamports: u64,
        budget_lamports: u64,
    },
    #[error("per-token cap reached for {token}: spent {spent_lamports} of {cap_lamports} cap")]
    PerTokenCap {
        token: String,
        spent_lamports: u64,
        cap_lamports: u64,
    },
    #[error("per-hour cap reached: {spent_lamports} of {cap_lamports} in the last 60 min")]
    PerHourCap {
        spent_lamports: u64,
        cap_lamports: u64,
    },
    #[error("zero amount requested")]
    ZeroAmount,
}

/// Snapshot of the budget configuration. Caller constructs this
/// when the user enables a bot session and passes it through to
/// every `check` call.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Total lamports the bot is allowed to spend in this session.
    pub session_budget_lamports: u64,
    /// Optional per-token cap. None = no per-token limit.
    pub per_token_cap_lamports: Option<u64>,
    /// Optional per-hour cap. None = no sliding-window limit.
    pub per_hour_cap_lamports: Option<u64>,
}

/// In-memory ledger of bot spend. Caller mutates via `record` after
/// a successful sign+submit. `check` is pure — does NOT mutate.
pub struct BudgetState {
    cfg: BudgetConfig,
    total_spent: u64,
    per_token: HashMap<String, u64>,
    /// (Instant, lamports) pairs, oldest first. The `check_at` /
    /// `record_at` API lets tests inject a clock; production uses
    /// `Instant::now`.
    recent: VecDeque<(Instant, u64)>,
}

impl BudgetState {
    pub fn new(cfg: BudgetConfig) -> Self {
        Self {
            cfg,
            total_spent: 0,
            per_token: HashMap::new(),
            recent: VecDeque::new(),
        }
    }

    /// Returns Ok(()) when an additional `lamports` spend on `token`
    /// would fit under every configured cap, evaluated at `now`.
    /// Pure — does NOT mutate state.
    pub fn check_at(&self, token: &str, lamports: u64, now: Instant) -> Result<(), BudgetError> {
        if lamports == 0 {
            return Err(BudgetError::ZeroAmount);
        }
        // Session.
        let session_after = self.total_spent.saturating_add(lamports);
        if session_after > self.cfg.session_budget_lamports {
            return Err(BudgetError::SessionExhausted {
                spent_lamports: self.total_spent,
                budget_lamports: self.cfg.session_budget_lamports,
            });
        }
        // Per-token.
        if let Some(cap) = self.cfg.per_token_cap_lamports {
            let token_spent = self.per_token.get(token).copied().unwrap_or(0);
            let token_after = token_spent.saturating_add(lamports);
            if token_after > cap {
                return Err(BudgetError::PerTokenCap {
                    token: token.to_string(),
                    spent_lamports: token_spent,
                    cap_lamports: cap,
                });
            }
        }
        // Per-hour: sum entries within the last hour.
        if let Some(cap) = self.cfg.per_hour_cap_lamports {
            let recent_sum = self.recent_sum_at(now);
            let hour_after = recent_sum.saturating_add(lamports);
            if hour_after > cap {
                return Err(BudgetError::PerHourCap {
                    spent_lamports: recent_sum,
                    cap_lamports: cap,
                });
            }
        }
        Ok(())
    }

    /// `check_at` with `Instant::now()`.
    pub fn check(&self, token: &str, lamports: u64) -> Result<(), BudgetError> {
        self.check_at(token, lamports, Instant::now())
    }

    /// Record a successful spend. Caller MUST call `check` first;
    /// `record` mutates regardless of cap-status (the auth check is
    /// the caller's, not ours).
    pub fn record_at(&mut self, token: &str, lamports: u64, at: Instant) {
        self.total_spent = self.total_spent.saturating_add(lamports);
        *self.per_token.entry(token.to_string()).or_insert(0) = self
            .per_token
            .get(token)
            .copied()
            .unwrap_or(0)
            .saturating_add(lamports);
        self.recent.push_back((at, lamports));
        self.evict_expired_at(at);
    }

    pub fn record(&mut self, token: &str, lamports: u64) {
        self.record_at(token, lamports, Instant::now());
    }

    /// Current sliding-window total. Evicts expired entries before
    /// summing — keeps memory bounded under sustained spend.
    fn recent_sum_at(&self, now: Instant) -> u64 {
        let window = Duration::from_secs(3600);
        self.recent
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= window)
            .map(|(_, n)| *n)
            .sum()
    }

    fn evict_expired_at(&mut self, now: Instant) {
        let window = Duration::from_secs(3600);
        while let Some(&(t, _)) = self.recent.front() {
            if now.saturating_duration_since(t) > window {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn total_spent(&self) -> u64 {
        self.total_spent
    }

    pub fn remaining(&self) -> u64 {
        self.cfg
            .session_budget_lamports
            .saturating_sub(self.total_spent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(session: u64, per_token: Option<u64>, per_hour: Option<u64>) -> BudgetConfig {
        BudgetConfig {
            session_budget_lamports: session,
            per_token_cap_lamports: per_token,
            per_hour_cap_lamports: per_hour,
        }
    }

    #[test]
    fn zero_amount_rejected() {
        let s = BudgetState::new(cfg(1_000_000_000, None, None));
        assert!(matches!(s.check("X", 0), Err(BudgetError::ZeroAmount)));
    }

    #[test]
    fn session_cap_enforced() {
        let mut s = BudgetState::new(cfg(100_000_000, None, None));
        assert!(s.check("X", 60_000_000).is_ok());
        s.record("X", 60_000_000);
        assert!(s.check("X", 30_000_000).is_ok());
        s.record("X", 30_000_000);
        // 90M + 20M would breach 100M cap.
        let err = s.check("X", 20_000_000).unwrap_err();
        assert!(matches!(err, BudgetError::SessionExhausted { .. }));
    }

    #[test]
    fn per_token_cap_enforced() {
        let mut s = BudgetState::new(cfg(1_000_000_000, Some(50_000_000), None));
        assert!(s.check("BONK", 30_000_000).is_ok());
        s.record("BONK", 30_000_000);
        // 30M + 25M > 50M token cap.
        let err = s.check("BONK", 25_000_000).unwrap_err();
        assert!(matches!(err, BudgetError::PerTokenCap { .. }));
        // But spending on a different token is fine.
        assert!(s.check("JUP", 25_000_000).is_ok());
    }

    #[test]
    fn per_hour_cap_with_clock_advance() {
        let mut s = BudgetState::new(cfg(1_000_000_000, None, Some(100_000_000)));
        let t0 = Instant::now();
        assert!(s.check_at("X", 60_000_000, t0).is_ok());
        s.record_at("X", 60_000_000, t0);
        // 30s later, still inside the hour.
        let t1 = t0 + Duration::from_secs(30);
        assert!(s.check_at("X", 30_000_000, t1).is_ok());
        s.record_at("X", 30_000_000, t1);
        // 60M + 30M = 90M, +20M = 110M > 100M cap.
        let err = s.check_at("X", 20_000_000, t1).unwrap_err();
        assert!(matches!(err, BudgetError::PerHourCap { .. }));
    }

    #[test]
    fn per_hour_cap_evicts_old_entries() {
        let mut s = BudgetState::new(cfg(10_000_000_000, None, Some(100_000_000)));
        let t0 = Instant::now();
        s.record_at("X", 80_000_000, t0);
        // 65 minutes later — the t0 entry should be outside the
        // window, hour-cap re-resets.
        let t1 = t0 + Duration::from_secs(65 * 60);
        assert!(s.check_at("X", 80_000_000, t1).is_ok());
    }

    #[test]
    fn remaining_reports_after_record() {
        let mut s = BudgetState::new(cfg(100_000_000, None, None));
        assert_eq!(s.remaining(), 100_000_000);
        s.record("X", 30_000_000);
        assert_eq!(s.total_spent(), 30_000_000);
        assert_eq!(s.remaining(), 70_000_000);
    }
}
