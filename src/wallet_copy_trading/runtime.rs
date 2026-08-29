use rust_decimal::Decimal;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize)]
pub struct WalletCopyTradingRuntimeSnapshot {
    pub initialized: bool,
    pub healthy: bool,
    pub entry_enabled: bool,
    pub whitelist_wallets: u64,
    pub signals_seen: u64,
    pub signals_rejected: u64,
    pub proposals: u64,
    pub fills: u64,
    pub open_positions: u64,
    pub nav_microusd: i64,
    pub cash_microusd: i64,
    pub last_poll_at_ms: i64,
    pub last_signal_at_ms: i64,
}

/// Shared lifecycle/control surface used by the standalone runner and polypulse TUI.
/// Disabling entries never disables exits, settlement, mark refresh, or reconciliation.
#[derive(Debug, Clone)]
pub struct WalletCopyTradingRuntime {
    shutdown: Arc<AtomicBool>,
    entry_enabled: Arc<AtomicBool>,
    initialized: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    whitelist_wallets: Arc<AtomicU64>,
    signals_seen: Arc<AtomicU64>,
    signals_rejected: Arc<AtomicU64>,
    proposals: Arc<AtomicU64>,
    fills: Arc<AtomicU64>,
    open_positions: Arc<AtomicU64>,
    nav_microusd: Arc<AtomicI64>,
    cash_microusd: Arc<AtomicI64>,
    last_poll_at_ms: Arc<AtomicI64>,
    last_signal_at_ms: Arc<AtomicI64>,
}

impl WalletCopyTradingRuntime {
    pub fn new(shutdown: Arc<AtomicBool>, entry_enabled: bool) -> Self {
        Self {
            shutdown,
            entry_enabled: Arc::new(AtomicBool::new(entry_enabled)),
            initialized: Arc::new(AtomicBool::new(false)),
            healthy: Arc::new(AtomicBool::new(false)),
            whitelist_wallets: Arc::new(AtomicU64::new(0)),
            signals_seen: Arc::new(AtomicU64::new(0)),
            signals_rejected: Arc::new(AtomicU64::new(0)),
            proposals: Arc::new(AtomicU64::new(0)),
            fills: Arc::new(AtomicU64::new(0)),
            open_positions: Arc::new(AtomicU64::new(0)),
            nav_microusd: Arc::new(AtomicI64::new(0)),
            cash_microusd: Arc::new(AtomicI64::new(0)),
            last_poll_at_ms: Arc::new(AtomicI64::new(0)),
            last_signal_at_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
    pub fn entry_enabled(&self) -> bool {
        self.entry_enabled.load(Ordering::Relaxed)
    }
    pub fn set_entry_enabled(&self, enabled: bool) {
        self.entry_enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn set_initialized(&self, value: bool) {
        self.initialized.store(value, Ordering::Relaxed);
    }
    pub fn set_healthy(&self, value: bool) {
        self.healthy.store(value, Ordering::Relaxed);
    }
    pub fn set_whitelist_wallets(&self, value: usize) {
        self.whitelist_wallets
            .store(value as u64, Ordering::Relaxed);
    }
    pub fn record_signal(&self, timestamp_ms: i64) {
        self.signals_seen.fetch_add(1, Ordering::Relaxed);
        self.last_signal_at_ms
            .store(timestamp_ms, Ordering::Relaxed);
    }
    pub fn record_rejection(&self) {
        self.signals_rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_proposal(&self) {
        self.proposals.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_fill(&self) {
        self.fills.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_poll(&self, timestamp_ms: i64) {
        self.last_poll_at_ms.store(timestamp_ms, Ordering::Relaxed);
    }
    pub fn set_portfolio(&self, nav: Decimal, cash: Decimal, open_positions: usize) {
        self.nav_microusd
            .store(decimal_microusd(nav), Ordering::Relaxed);
        self.cash_microusd
            .store(decimal_microusd(cash), Ordering::Relaxed);
        self.open_positions
            .store(open_positions as u64, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> WalletCopyTradingRuntimeSnapshot {
        WalletCopyTradingRuntimeSnapshot {
            initialized: self.initialized.load(Ordering::Relaxed),
            healthy: self.healthy.load(Ordering::Relaxed),
            entry_enabled: self.entry_enabled(),
            whitelist_wallets: self.whitelist_wallets.load(Ordering::Relaxed),
            signals_seen: self.signals_seen.load(Ordering::Relaxed),
            signals_rejected: self.signals_rejected.load(Ordering::Relaxed),
            proposals: self.proposals.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
            open_positions: self.open_positions.load(Ordering::Relaxed),
            nav_microusd: self.nav_microusd.load(Ordering::Relaxed),
            cash_microusd: self.cash_microusd.load(Ordering::Relaxed),
            last_poll_at_ms: self.last_poll_at_ms.load(Ordering::Relaxed),
            last_signal_at_ms: self.last_signal_at_ms.load(Ordering::Relaxed),
        }
    }
}

fn decimal_microusd(value: Decimal) -> i64 {
    (value * Decimal::from(1_000_000))
        .round()
        .to_string()
        .parse()
        .unwrap_or_else(|_| {
            if value.is_sign_negative() {
                i64::MIN
            } else {
                i64::MAX
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn entry_pause_does_not_request_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let runtime = WalletCopyTradingRuntime::new(shutdown, true);
        runtime.set_entry_enabled(false);
        runtime.set_portfolio(dec!(12.5), dec!(10), 2);
        let snapshot = runtime.snapshot();
        assert!(!snapshot.entry_enabled);
        assert!(!runtime.shutdown_requested());
        assert_eq!(snapshot.nav_microusd, 12_500_000);
    }
}
