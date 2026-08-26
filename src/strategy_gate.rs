//! Pure pre-trade guards for snapshot freshness and duplicate signals.

use dashmap::DashMap;
use polymarket_client_sdk::types::{Decimal, B256};
use std::time::{Duration, Instant};

pub fn validate_book_timestamps(
    yes_timestamp_ms: i64,
    no_timestamp_ms: i64,
    now_timestamp_ms: i64,
    max_age_ms: i64,
    max_skew_ms: i64,
) -> anyhow::Result<()> {
    let newest = yes_timestamp_ms.max(no_timestamp_ms);
    let oldest = yes_timestamp_ms.min(no_timestamp_ms);
    if newest > now_timestamp_ms + max_skew_ms {
        anyhow::bail!("orderbook timestamp is unexpectedly in the future");
    }
    if now_timestamp_ms.saturating_sub(oldest) > max_age_ms {
        anyhow::bail!("orderbook pair is stale");
    }
    if newest.saturating_sub(oldest) > max_skew_ms {
        anyhow::bail!("YES/NO orderbook snapshots are too far apart");
    }
    Ok(())
}

pub struct SignalDeduplicator {
    cooldown: Duration,
    last: DashMap<B256, (Decimal, Decimal, Instant)>,
}

impl SignalDeduplicator {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last: DashMap::new(),
        }
    }

    /// Returns true once for a market/price signal, then suppresses the identical
    /// signal until the cooldown elapses. A changed price is immediately eligible.
    pub fn admit(&self, market: B256, yes: Decimal, no: Decimal, now: Instant) -> bool {
        if let Some(previous) = self.last.get(&market) {
            if previous.0 == yes
                && previous.1 == no
                && now.saturating_duration_since(previous.2) < self.cooldown
            {
                return false;
            }
        }
        self.last.insert(market, (yes, no, now));
        true
    }

    pub fn clear(&self) {
        self.last.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn rejects_stale_skewed_and_future_books() {
        assert!(validate_book_timestamps(9_500, 9_700, 10_000, 1_000, 500).is_ok());
        assert!(validate_book_timestamps(8_000, 9_700, 10_000, 1_000, 500).is_err());
        assert!(validate_book_timestamps(9_000, 9_700, 10_000, 2_000, 500).is_err());
        assert!(validate_book_timestamps(10_600, 10_600, 10_000, 2_000, 500).is_err());
    }

    #[test]
    fn suppresses_only_identical_signals_during_cooldown() {
        let gate = SignalDeduplicator::new(Duration::from_secs(3));
        let market = B256::repeat_byte(3);
        let now = Instant::now();
        assert!(gate.admit(market, dec!(0.4), dec!(0.5), now));
        assert!(!gate.admit(market, dec!(0.4), dec!(0.5), now + Duration::from_secs(1)));
        assert!(gate.admit(market, dec!(0.39), dec!(0.5), now + Duration::from_secs(1)));
        assert!(gate.admit(market, dec!(0.39), dec!(0.5), now + Duration::from_secs(5)));
    }
}
