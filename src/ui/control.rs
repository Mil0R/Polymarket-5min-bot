use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    Arbitrage,
    WinnerBuy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyDisplayState {
    Running,
    Paused,
    Blocked,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlResult {
    Changed {
        strategy: StrategyKind,
        enabled: bool,
    },
    ConfirmationRequired {
        strategy: StrategyKind,
    },
    Rejected(String),
}

struct PendingConfirmation {
    strategy: StrategyKind,
    expires_at: Instant,
}

pub struct StrategyControls {
    arbitrage_enabled: AtomicBool,
    winner_buy_enabled: AtomicBool,
    winner_buy_available: bool,
    live_mode: bool,
    resume_allowed: AtomicBool,
    blocked_reason: Mutex<String>,
    confirmation: Mutex<Option<PendingConfirmation>>,
}

impl StrategyControls {
    pub fn new(
        arbitrage_enabled: bool,
        winner_buy_enabled: bool,
        winner_buy_available: bool,
        live_mode: bool,
    ) -> Self {
        Self {
            arbitrage_enabled: AtomicBool::new(arbitrage_enabled),
            winner_buy_enabled: AtomicBool::new(winner_buy_enabled && winner_buy_available),
            winner_buy_available,
            live_mode,
            resume_allowed: AtomicBool::new(false),
            blocked_reason: Mutex::new("initializing".into()),
            confirmation: Mutex::new(None),
        }
    }

    pub fn arbitrage_enabled(&self) -> bool {
        self.arbitrage_enabled.load(Ordering::Relaxed)
    }

    pub fn winner_buy_enabled(&self) -> bool {
        self.winner_buy_enabled.load(Ordering::Relaxed)
    }

    pub fn set_resume_allowed(&self, allowed: bool, reason: impl Into<String>) {
        self.resume_allowed.store(allowed, Ordering::Relaxed);
        *self.blocked_reason.lock().expect("strategy reason lock") = reason.into();
    }

    pub fn blocked_reason(&self) -> String {
        self.blocked_reason
            .lock()
            .expect("strategy reason lock")
            .clone()
    }

    pub fn state(&self, strategy: StrategyKind) -> StrategyDisplayState {
        if matches!(strategy, StrategyKind::WinnerBuy) && !self.winner_buy_available {
            return StrategyDisplayState::Unavailable;
        }
        let enabled = match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled(),
            StrategyKind::WinnerBuy => self.winner_buy_enabled(),
        };
        if !enabled {
            StrategyDisplayState::Paused
        } else if !self.resume_allowed.load(Ordering::Relaxed) {
            StrategyDisplayState::Blocked
        } else {
            StrategyDisplayState::Running
        }
    }

    pub fn toggle(&self, strategy: StrategyKind) -> ControlResult {
        if matches!(strategy, StrategyKind::WinnerBuy) && !self.winner_buy_available {
            return ControlResult::Rejected("Winner Buy dependencies are unavailable".into());
        }
        let enabled = match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled(),
            StrategyKind::WinnerBuy => self.winner_buy_enabled(),
        };
        if enabled {
            self.set_enabled(strategy, false);
            return ControlResult::Changed {
                strategy,
                enabled: false,
            };
        }
        if !self.resume_allowed.load(Ordering::Relaxed) {
            return ControlResult::Rejected(self.blocked_reason());
        }
        if self.live_mode {
            *self
                .confirmation
                .lock()
                .expect("strategy confirmation lock") = Some(PendingConfirmation {
                strategy,
                expires_at: Instant::now() + Duration::from_secs(5),
            });
            return ControlResult::ConfirmationRequired { strategy };
        }
        self.set_enabled(strategy, true);
        ControlResult::Changed {
            strategy,
            enabled: true,
        }
    }

    pub fn confirm(&self) -> ControlResult {
        let Some(pending) = self
            .confirmation
            .lock()
            .expect("strategy confirmation lock")
            .take()
        else {
            return ControlResult::Rejected("No strategy enable is awaiting confirmation".into());
        };
        if Instant::now() > pending.expires_at {
            return ControlResult::Rejected("Strategy enable confirmation expired".into());
        }
        if !self.resume_allowed.load(Ordering::Relaxed) {
            return ControlResult::Rejected(self.blocked_reason());
        }
        self.set_enabled(pending.strategy, true);
        ControlResult::Changed {
            strategy: pending.strategy,
            enabled: true,
        }
    }

    pub fn pause_all(&self) {
        self.arbitrage_enabled.store(false, Ordering::Relaxed);
        self.winner_buy_enabled.store(false, Ordering::Relaxed);
        *self
            .confirmation
            .lock()
            .expect("strategy confirmation lock") = None;
    }

    fn set_enabled(&self, strategy: StrategyKind, enabled: bool) {
        match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled.store(enabled, Ordering::Relaxed),
            StrategyKind::WinnerBuy => self.winner_buy_enabled.store(enabled, Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_toggles_immediately_and_pause_all_is_fail_closed() {
        let controls = StrategyControls::new(true, true, true, false);
        controls.set_resume_allowed(true, "ready");
        assert!(matches!(
            controls.toggle(StrategyKind::WinnerBuy),
            ControlResult::Changed { enabled: false, .. }
        ));
        assert!(matches!(
            controls.toggle(StrategyKind::WinnerBuy),
            ControlResult::Changed { enabled: true, .. }
        ));
        controls.pause_all();
        assert!(!controls.arbitrage_enabled());
        assert!(!controls.winner_buy_enabled());
    }

    #[test]
    fn live_resume_requires_confirmation_and_health() {
        let controls = StrategyControls::new(false, false, true, true);
        assert!(matches!(
            controls.toggle(StrategyKind::Arbitrage),
            ControlResult::Rejected(_)
        ));
        controls.set_resume_allowed(true, "ready");
        assert!(matches!(
            controls.toggle(StrategyKind::Arbitrage),
            ControlResult::ConfirmationRequired { .. }
        ));
        assert!(matches!(
            controls.confirm(),
            ControlResult::Changed { enabled: true, .. }
        ));
    }
}
