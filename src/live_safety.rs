//! Process-wide live/canary safety gate.

use crate::trading::executor::{ExecutionQuote, OrderPairResult};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Mutex;

pub const ABSOLUTE_CANARY_PAIR_LIMIT_USDC: f64 = 10.0;
pub const ABSOLUTE_CANARY_TOTAL_LIMIT_USDC: f64 = 20.0;
pub const ABSOLUTE_CANARY_LOSS_LIMIT_USDC: f64 = 5.0;

#[derive(Debug, Clone)]
pub struct CanaryPolicy {
    pub enabled: bool,
    pub max_pair_notional: Decimal,
    pub max_total_notional: Decimal,
    pub max_loss_budget: Decimal,
    pub max_pairs_per_window: u32,
    pub max_pairs_total: u32,
    /// Consecutive one-sided fills that trip the gate (0 = disabled).
    pub max_consecutive_one_sided: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSafetySnapshot {
    pub tripped: bool,
    pub reason: Option<String>,
    pub pairs_started: u32,
    pub planned_notional: Decimal,
    pub loss_budget_used: Decimal,
}

#[derive(Default)]
struct LiveSafetyState {
    tripped: bool,
    reason: Option<String>,
    current_window: Option<i64>,
    pairs_in_window: u32,
    pairs_started: u32,
    planned_notional: Decimal,
    loss_budget_used: Decimal,
    consecutive_one_sided: u32,
}

pub struct LiveSafetyGuard {
    policy: CanaryPolicy,
    state: Mutex<LiveSafetyState>,
}

impl LiveSafetyGuard {
    pub fn new(policy: CanaryPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(LiveSafetyState::default()),
        }
    }

    pub fn canary_enabled(&self) -> bool {
        self.policy.enabled
    }

    /// Conservatively consumes a canary attempt before any account write.
    /// Counters are intentionally not rolled back after failures.
    pub fn try_start_pair(&self, window: i64, quote: &ExecutionQuote) -> Result<(), String> {
        let mut state = self.state.lock().expect("live safety lock poisoned");
        if state.tripped {
            return Err(state
                .reason
                .clone()
                .unwrap_or_else(|| "live safety gate tripped".into()));
        }
        if !self.policy.enabled {
            return Ok(());
        }
        if state.current_window != Some(window) {
            state.current_window = Some(window);
            state.pairs_in_window = 0;
        }
        if quote.total_cost > self.policy.max_pair_notional {
            return Err(format!(
                "canary pair notional {} exceeds {}",
                quote.total_cost, self.policy.max_pair_notional
            ));
        }
        if state.planned_notional + quote.total_cost > self.policy.max_total_notional {
            return Err(format!(
                "canary total notional would exceed {}",
                self.policy.max_total_notional
            ));
        }
        if state.pairs_in_window >= self.policy.max_pairs_per_window {
            return Err("canary pair limit for this window reached".into());
        }
        if state.pairs_started >= self.policy.max_pairs_total {
            return Err("canary process-wide pair limit reached".into());
        }
        state.pairs_in_window += 1;
        state.pairs_started += 1;
        state.planned_notional += quote.total_cost;
        Ok(())
    }

    pub fn record_result(&self, result: &OrderPairResult) {
        let matched = result.yes_filled.min(result.no_filled);
        let paired_loss = ((result.yes_price + result.no_price - dec!(1)).max(dec!(0))) * matched;
        let residual_loss = (result.yes_filled - matched).max(dec!(0)) * result.yes_price
            + (result.no_filled - matched).max(dec!(0)) * result.no_price;
        let mut state = self.state.lock().expect("live safety lock poisoned");
        state.loss_budget_used += paired_loss + residual_loss;
        let one_sided = (result.yes_filled > dec!(0)) != (result.no_filled > dec!(0));
        state.consecutive_one_sided = if one_sided {
            state.consecutive_one_sided + 1
        } else {
            0
        };
        if result.write_uncertain {
            trip_locked(&mut state, "CLOB write outcome is uncertain");
        } else if one_sided && self.policy.max_consecutive_one_sided > 0
            && state.consecutive_one_sided >= self.policy.max_consecutive_one_sided
        {
            let reason = format!(
                "{} consecutive one-sided fills; pausing to avoid unhedged exposure",
                state.consecutive_one_sided
            );
            trip_locked(&mut state, &reason);
        } else if self.policy.enabled && state.loss_budget_used >= self.policy.max_loss_budget {
            let reason = format!(
                "canary loss budget {} reached limit {}",
                state.loss_budget_used, self.policy.max_loss_budget
            );
            trip_locked(&mut state, &reason);
        }
    }

    pub fn trip(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut state = self.state.lock().expect("live safety lock poisoned");
        trip_locked(&mut state, &reason);
    }

    pub fn is_tripped(&self) -> bool {
        self.state
            .lock()
            .expect("live safety lock poisoned")
            .tripped
    }

    pub fn snapshot(&self) -> LiveSafetySnapshot {
        let state = self.state.lock().expect("live safety lock poisoned");
        LiveSafetySnapshot {
            tripped: state.tripped,
            reason: state.reason.clone(),
            pairs_started: state.pairs_started,
            planned_notional: state.planned_notional,
            loss_budget_used: state.loss_budget_used,
        }
    }
}

fn trip_locked(state: &mut LiveSafetyState, reason: &str) {
    if !state.tripped {
        state.tripped = true;
        state.reason = Some(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CanaryPolicy {
        CanaryPolicy {
            enabled: true,
            max_pair_notional: dec!(3),
            max_total_notional: dec!(6),
            max_loss_budget: dec!(1),
            max_pairs_per_window: 1,
            max_pairs_total: 2,
            max_consecutive_one_sided: 0,
        }
    }

    fn quote(cost: Decimal) -> ExecutionQuote {
        ExecutionQuote {
            yes_price: dec!(0.45),
            no_price: dec!(0.50),
            size: dec!(2),
            total_cost: cost,
            edge_per_share: dec!(0.05),
        }
    }

    fn result(yes: Decimal, no: Decimal, uncertain: bool) -> OrderPairResult {
        OrderPairResult {
            pair_id: "pair".into(),
            yes_order_id: "yes".into(),
            no_order_id: "no".into(),
            yes_filled: yes,
            no_filled: no,
            yes_size: dec!(2),
            no_size: dec!(2),
            yes_price: dec!(0.45),
            no_price: dec!(0.50),
            success: yes == no,
            write_uncertain: uncertain,
        }
    }

    #[test]
    fn enforces_pair_window_and_process_limits() {
        let guard = LiveSafetyGuard::new(policy());
        assert!(guard.try_start_pair(1, &quote(dec!(2))).is_ok());
        assert!(guard.try_start_pair(1, &quote(dec!(2))).is_err());
        assert!(guard.try_start_pair(2, &quote(dec!(4))).is_err());
        assert!(guard.try_start_pair(2, &quote(dec!(2))).is_ok());
        assert!(guard.try_start_pair(3, &quote(dec!(1))).is_err());
    }

    #[test]
    fn one_sided_downside_and_uncertain_writes_trip_gate() {
        let guard = LiveSafetyGuard::new(policy());
        guard.record_result(&result(dec!(3), dec!(0), false));
        assert!(guard.is_tripped());

        let uncertain = LiveSafetyGuard::new(policy());
        uncertain.record_result(&result(dec!(0), dec!(0), true));
        assert_eq!(
            uncertain.snapshot().reason.as_deref(),
            Some("CLOB write outcome is uncertain")
        );
    }

    #[test]
    fn consecutive_one_sided_fills_trip_gate_even_without_canary() {
        let policy = CanaryPolicy {
            enabled: false,
            max_pair_notional: dec!(3),
            max_total_notional: dec!(6),
            max_loss_budget: dec!(1),
            max_pairs_per_window: 1,
            max_pairs_total: 2,
            max_consecutive_one_sided: 2,
        };
        let guard = LiveSafetyGuard::new(policy);
        // First one-sided fill: not yet tripped.
        guard.record_result(&result(dec!(2), dec!(0), false));
        assert!(!guard.is_tripped());
        // Second consecutive one-sided fill: trips.
        guard.record_result(&result(dec!(0), dec!(1.5), false));
        assert!(guard.is_tripped());
        assert!(guard.snapshot().reason.unwrap().contains("one-sided"));
    }

    #[test]
    fn paired_fill_resets_one_sided_counter() {
        let policy = CanaryPolicy {
            enabled: false,
            max_pair_notional: dec!(3),
            max_total_notional: dec!(6),
            max_loss_budget: dec!(1),
            max_pairs_per_window: 1,
            max_pairs_total: 2,
            max_consecutive_one_sided: 2,
        };
        let guard = LiveSafetyGuard::new(policy);
        guard.record_result(&result(dec!(2), dec!(0), false));
        guard.record_result(&result(dec!(1.5), dec!(1.5), false));
        assert!(!guard.is_tripped());
        // One-sided again: counter restarted from 0, so only 1 so far.
        guard.record_result(&result(dec!(0), dec!(1), false));
        assert!(!guard.is_tripped());
    }
}
