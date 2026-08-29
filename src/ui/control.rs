use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::wallet_copy_trading::WalletCopyTradingRuntime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    BinaryComplementArbitrage,
    LateResolutionWinnerTrading,
    WalletCopyTrading,
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
    binary_complement_arbitrage_enabled: AtomicBool,
    late_resolution_winner_trading_enabled: AtomicBool,
    late_resolution_winner_trading_available: bool,
    wallet_copy_trading_available: bool,
    copy_runtime: Option<WalletCopyTradingRuntime>,
    live_mode: bool,
    resume_allowed: AtomicBool,
    blocked_reason: Mutex<String>,
    confirmation: Mutex<Option<PendingConfirmation>>,
}

impl StrategyControls {
    pub fn new(
        binary_complement_arbitrage_enabled: bool,
        late_resolution_winner_trading_enabled: bool,
        late_resolution_winner_trading_available: bool,
        wallet_copy_trading_available: bool,
        copy_runtime: Option<WalletCopyTradingRuntime>,
        live_mode: bool,
    ) -> Self {
        Self {
            binary_complement_arbitrage_enabled: AtomicBool::new(
                binary_complement_arbitrage_enabled,
            ),
            late_resolution_winner_trading_enabled: AtomicBool::new(
                late_resolution_winner_trading_enabled && late_resolution_winner_trading_available,
            ),
            late_resolution_winner_trading_available,
            wallet_copy_trading_available,
            copy_runtime,
            live_mode,
            resume_allowed: AtomicBool::new(false),
            blocked_reason: Mutex::new("initializing".into()),
            confirmation: Mutex::new(None),
        }
    }

    pub fn binary_complement_arbitrage_enabled(&self) -> bool {
        self.binary_complement_arbitrage_enabled
            .load(Ordering::Relaxed)
    }

    pub fn late_resolution_winner_trading_enabled(&self) -> bool {
        self.late_resolution_winner_trading_enabled
            .load(Ordering::Relaxed)
    }

    pub fn wallet_copy_trading_enabled(&self) -> bool {
        self.copy_runtime
            .as_ref()
            .is_some_and(WalletCopyTradingRuntime::entry_enabled)
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
        if matches!(strategy, StrategyKind::LateResolutionWinnerTrading)
            && !self.late_resolution_winner_trading_available
        {
            return StrategyDisplayState::Unavailable;
        }
        if matches!(strategy, StrategyKind::WalletCopyTrading)
            && !self.wallet_copy_trading_available
        {
            return StrategyDisplayState::Unavailable;
        }
        if matches!(strategy, StrategyKind::WalletCopyTrading) {
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
            StrategyKind::BinaryComplementArbitrage => self.binary_complement_arbitrage_enabled(),
            StrategyKind::LateResolutionWinnerTrading => {
                self.late_resolution_winner_trading_enabled()
            }
            StrategyKind::WalletCopyTrading => self.wallet_copy_trading_enabled(),
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
        if matches!(strategy, StrategyKind::LateResolutionWinnerTrading)
            && !self.late_resolution_winner_trading_available
        {
            return ControlResult::Rejected(
                "Late-Resolution Winner Trading dependencies are unavailable".into(),
            );
        }
        if matches!(strategy, StrategyKind::WalletCopyTrading)
            && !self.wallet_copy_trading_available
        {
            return ControlResult::Rejected(
                "Wallet-Copy Trading is available only when its PAPER service is initialized"
                    .into(),
            );
        }
        let enabled = match strategy {
            StrategyKind::BinaryComplementArbitrage => self.binary_complement_arbitrage_enabled(),
            StrategyKind::LateResolutionWinnerTrading => {
                self.late_resolution_winner_trading_enabled()
            }
            StrategyKind::WalletCopyTrading => self.wallet_copy_trading_enabled(),
        };
        if enabled {
            self.set_enabled(strategy, false);
            return ControlResult::Changed {
                strategy,
                enabled: false,
            };
        }
        if matches!(strategy, StrategyKind::WalletCopyTrading) {
            let ready = self.copy_runtime.as_ref().is_some_and(|runtime| {
                let snapshot = runtime.snapshot();
                snapshot.initialized && snapshot.healthy
            });
            if !ready {
                return ControlResult::Rejected(
                    "Wallet-Copy Trading service is not healthy".into(),
                );
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
        self.binary_complement_arbitrage_enabled
            .store(false, Ordering::Relaxed);
        self.late_resolution_winner_trading_enabled
            .store(false, Ordering::Relaxed);
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
            StrategyKind::BinaryComplementArbitrage => self
                .binary_complement_arbitrage_enabled
                .store(enabled, Ordering::Relaxed),
            StrategyKind::LateResolutionWinnerTrading => self
                .late_resolution_winner_trading_enabled
                .store(enabled, Ordering::Relaxed),
            StrategyKind::WalletCopyTrading => {
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
        let runtime =
            WalletCopyTradingRuntime::new(std::sync::Arc::new(AtomicBool::new(false)), true);
        let controls = StrategyControls::new(true, true, true, true, Some(runtime), false);
        controls.set_resume_allowed(true, "ready");
        assert!(matches!(
            controls.toggle(StrategyKind::LateResolutionWinnerTrading),
            ControlResult::Changed { enabled: false, .. }
        ));
        assert!(matches!(
            controls.toggle(StrategyKind::LateResolutionWinnerTrading),
            ControlResult::Changed { enabled: true, .. }
        ));
        controls.pause_all();
        assert!(!controls.binary_complement_arbitrage_enabled());
        assert!(!controls.late_resolution_winner_trading_enabled());
        assert!(!controls.wallet_copy_trading_enabled());
    }

    #[test]
    fn live_resume_requires_confirmation_and_health() {
        let controls = StrategyControls::new(false, false, true, false, None, true);
        assert!(matches!(
            controls.toggle(StrategyKind::BinaryComplementArbitrage),
            ControlResult::Rejected(_)
        ));
        controls.set_resume_allowed(true, "ready");
        assert!(matches!(
            controls.toggle(StrategyKind::BinaryComplementArbitrage),
            ControlResult::ConfirmationRequired { .. }
        ));
        assert!(matches!(
            controls.confirm(),
            ControlResult::Changed { enabled: true, .. }
        ));
    }
}
