use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::copy_trading::CopyTradingRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    Arbitrage,
    WinnerBuy,
    CopyTrading,
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
    copy_trading_available: bool,
    copy_runtime: Option<CopyTradingRuntime>,
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
        copy_trading_available: bool,
        copy_runtime: Option<CopyTradingRuntime>,
        live_mode: bool,
    ) -> Self {
        Self {
            arbitrage_enabled: AtomicBool::new(arbitrage_enabled),
            winner_buy_enabled: AtomicBool::new(winner_buy_enabled && winner_buy_available),
            winner_buy_available,
            copy_trading_available,
            copy_runtime,
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

    pub fn copy_trading_enabled(&self) -> bool {
        self.copy_runtime
            .as_ref()
            .is_some_and(CopyTradingRuntime::entry_enabled)
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
        if matches!(strategy, StrategyKind::CopyTrading) && !self.copy_trading_available {
            return StrategyDisplayState::Unavailable;
        }
        if matches!(strategy, StrategyKind::CopyTrading) {
            let Some(runtime) = &self.copy_runtime else {
                return StrategyDisplayState::Unavailable;
            };
            let snapshot = runtime.snapshot();
            return if !snapshot.initialized || (snapshot.entry_enabled && !snapshot.healthy) {
                StrategyDisplayState::Blocked
            } else if !snapshot.entry_enabled {
                StrategyDisplayState::Paused
            } else {
                StrategyDisplayState::Running
            };
        }
        let enabled = match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled(),
            StrategyKind::WinnerBuy => self.winner_buy_enabled(),
            StrategyKind::CopyTrading => self.copy_trading_enabled(),
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
        if matches!(strategy, StrategyKind::CopyTrading) && !self.copy_trading_available {
            return ControlResult::Rejected(
                "Copy Trading is available only when its PAPER service is initialized".into(),
            );
        }
        let enabled = match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled(),
            StrategyKind::WinnerBuy => self.winner_buy_enabled(),
            StrategyKind::CopyTrading => self.copy_trading_enabled(),
        };
        if enabled {
            self.set_enabled(strategy, false);
            return ControlResult::Changed {
                strategy,
                enabled: false,
            };
        }
        if matches!(strategy, StrategyKind::CopyTrading) {
            let ready = self.copy_runtime.as_ref().is_some_and(|runtime| {
                let snapshot = runtime.snapshot();
                snapshot.initialized && snapshot.healthy
            });
            if !ready {
                return ControlResult::Rejected("Copy Trading service is not healthy".into());
            }
            self.set_enabled(strategy, true);
            return ControlResult::Changed {
                strategy,
                enabled: true,
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
        if let Some(runtime) = &self.copy_runtime {
            runtime.set_entry_enabled(false);
        }
        *self
            .confirmation
            .lock()
            .expect("strategy confirmation lock") = None;
    }

    fn set_enabled(&self, strategy: StrategyKind, enabled: bool) {
        match strategy {
            StrategyKind::Arbitrage => self.arbitrage_enabled.store(enabled, Ordering::Relaxed),
            StrategyKind::WinnerBuy => self.winner_buy_enabled.store(enabled, Ordering::Relaxed),
            StrategyKind::CopyTrading => {
                if let Some(runtime) = &self.copy_runtime {
                    runtime.set_entry_enabled(enabled);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_toggles_immediately_and_pause_all_is_fail_closed() {
        let runtime = CopyTradingRuntime::new(std::sync::Arc::new(AtomicBool::new(false)), true);
        let controls = StrategyControls::new(true, true, true, true, Some(runtime), false);
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
        assert!(!controls.copy_trading_enabled());
    }

    #[test]
    fn live_resume_requires_confirmation_and_health() {
        let controls = StrategyControls::new(false, false, true, false, None, true);
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
