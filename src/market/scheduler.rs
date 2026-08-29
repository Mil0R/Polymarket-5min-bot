use anyhow::Result;
use chrono::{DateTime, Utc};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::discoverer::{MarketDiscoverer, MarketInfo};

pub struct MarketScheduler {
    discoverer: MarketDiscoverer,
    refresh_advance_secs: u64,
}

impl MarketScheduler {
    pub fn new(discoverer: MarketDiscoverer, refresh_advance_secs: u64) -> Self {
        Self {
            discoverer,
            refresh_advance_secs,
        }
    }

    /// Current window start timestamp, delegated to the discoverer's period.
    pub fn calculate_current_window_timestamp(&self, now: DateTime<Utc>) -> i64 {
        self.discoverer.calculate_current_window_timestamp(now)
    }

    /// Next window start timestamp. Kept separate from the wake-up instant so
    /// an early refresh never rounds back down to the expiring window.
    pub fn calculate_next_window_timestamp(&self, now: DateTime<Utc>) -> i64 {
        self.discoverer.calculate_next_window_timestamp(now)
    }

    /// Raw markets by slug — used for settlement checks of late-resolution-winner-trading positions.
    #[allow(dead_code)]
    pub async fn fetch_markets_by_slug(
        &self,
        slugs: Vec<String>,
    ) -> Result<Vec<polymarket_client_sdk::gamma::types::response::Market>> {
        self.discoverer.fetch_markets_by_slug(slugs).await
    }

    /// Closed-market lookup used exclusively for terminal outcome resolution.
    pub async fn fetch_closed_markets_by_slug(
        &self,
        slugs: Vec<String>,
    ) -> Result<Vec<polymarket_client_sdk::gamma::types::response::Market>> {
        self.discoverer.fetch_closed_markets_by_slug(slugs).await
    }

    /// Calculate wait time until next window
    pub fn calculate_wait_time(&self, now: DateTime<Utc>) -> Duration {
        let next_window_ts = self.discoverer.calculate_next_window_timestamp(now);
        let next_window = DateTime::from_timestamp(next_window_ts, 0).expect("Invalid timestamp");

        // Query a few seconds early so markets are created
        let wait_duration = next_window
            .signed_duration_since(now)
            .to_std()
            .unwrap_or(Duration::ZERO)
            .saturating_sub(Duration::from_secs(self.refresh_advance_secs));

        wait_duration.max(Duration::ZERO)
    }

    /// Fetch markets for current window immediately, or wait for next on failure
    pub async fn get_markets_immediately_or_wait(&self) -> Result<Vec<MarketInfo>> {
        // Try to fetch current window markets first
        let now = Utc::now();
        let current_timestamp = self.discoverer.calculate_current_window_timestamp(now);
        let next_timestamp = self.discoverer.calculate_next_window_timestamp(now);

        // If current and next window same (shouldn't happen for 5m), use wait logic
        if current_timestamp == next_timestamp {
            return self.wait_for_next_window().await;
        }

        info!("Fetching markets for current window");
        match self
            .discoverer
            .get_markets_for_timestamp(current_timestamp)
            .await
        {
            Ok(markets) => {
                if !markets.is_empty() {
                    info!(count = markets.len(), "Found markets for current window");
                    return Ok(markets);
                }
                // No markets: maybe not created yet; retry with short interval (5m markets usually ready in seconds)
                // Calling wait_for_next_window would skip to next boundary and miss this window
                const RETRY_SECS: u64 = 2;
                const MAX_RETRY_SECS: u64 = 90; // Max retry ~90s
                let mut elapsed = 0u64;
                while elapsed < MAX_RETRY_SECS {
                    info!(
                        "Current window empty, retrying in {}s (waited {}s)",
                        RETRY_SECS, elapsed
                    );
                    sleep(Duration::from_secs(RETRY_SECS)).await;
                    elapsed += RETRY_SECS;
                    match self
                        .discoverer
                        .get_markets_for_timestamp(current_timestamp)
                        .await
                    {
                        Ok(markets) if !markets.is_empty() => {
                            info!(count = markets.len(), "Retry succeeded, found markets");
                            return Ok(markets);
                        }
                        _ => {}
                    }
                }
                // Retry timed out, wait for next window
                warn!(
                    "No markets after {}s retry, waiting for next window",
                    MAX_RETRY_SECS
                );
                self.wait_for_next_window().await
            }
            Err(e) => {
                warn!(error = %e, "Failed to fetch current window markets, waiting for next");
                self.wait_for_next_window().await
            }
        }
    }

    /// Wait for next 5-minute window and fetch markets
    pub async fn wait_for_next_window(&self) -> Result<Vec<MarketInfo>> {
        let started_at = Utc::now();
        let target_timestamp = self.calculate_next_window_timestamp(started_at);
        let wait_time = self.calculate_wait_time(started_at);
        if wait_time > Duration::ZERO {
            info!(
                wait_secs = wait_time.as_secs(),
                target_timestamp, "Waiting for next window"
            );
            sleep(wait_time).await;
        }

        loop {
            // Always query the captured next-window target. When refresh wakes
            // a few seconds early, `Utc::now()` still belongs to the old window.
            match self
                .discoverer
                .get_markets_for_timestamp(target_timestamp)
                .await
            {
                Ok(markets) => {
                    if !markets.is_empty() {
                        info!(count = markets.len(), target_timestamp, "Found new markets");
                        return Ok(markets);
                    }
                    // Markets not created yet, wait and retry
                    info!("Markets not created yet, waiting to retry...");
                    sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    error!(error = %e, "Failed to fetch markets, retrying...");
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn early_refresh_keeps_the_next_window_as_query_target() {
        let scheduler = MarketScheduler::new(MarketDiscoverer::new(vec!["btc".into()], 5), 5);
        let now = Utc.with_ymd_and_hms(2026, 8, 20, 12, 4, 55).unwrap();

        assert_eq!(
            scheduler.calculate_current_window_timestamp(now),
            1_787_227_200
        );
        assert_eq!(
            scheduler.calculate_next_window_timestamp(now),
            1_787_227_500
        );
        assert_eq!(scheduler.calculate_wait_time(now), Duration::ZERO);
    }
}
