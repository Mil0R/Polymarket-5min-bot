//! Per-window metadata, safety state, task ownership and orderly shutdown.

use crate::market::discoverer::{window_seconds, MarketInfo};
use crate::window::WindowScope;
use chrono::{DateTime, Utc};
use polymarket_client_sdk::types::{B256, U256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::task::{JoinHandle, JoinSet};
use tracing::warn;

pub struct WindowTaskSet {
    trades: JoinSet<()>,
    wind_down: Option<JoinHandle<()>>,
}

pub struct WindowSession {
    timestamp: i64,
    end: DateTime<Utc>,
    label: String,
    market_tokens: HashMap<B256, (U256, U256)>,
    scope: Arc<WindowScope>,
    tasks: WindowTaskSet,
    wind_down_started: bool,
    user_stream_failed: bool,
    started_at: Instant,
}

impl WindowSession {
    pub fn new(
        markets: &[MarketInfo],
        now: DateTime<Utc>,
        period_minutes: u64,
    ) -> anyhow::Result<Self> {
        let first = markets
            .first()
            .ok_or_else(|| anyhow::anyhow!("cannot create an empty window session"))?;
        let secs = window_seconds(period_minutes);
        let timestamp = (now.timestamp() / secs) * secs;
        let end = DateTime::from_timestamp(timestamp + secs, 0)
            .ok_or_else(|| anyhow::anyhow!("invalid window timestamp {timestamp}"))?;
        let market_tokens: HashMap<_, _> = markets
            .iter()
            .map(|market| (market.market_id, (market.yes_token_id, market.no_token_id)))
            .collect();
        let label = format!(
            "{}-updown-{}m",
            first.crypto_symbol.to_ascii_lowercase(),
            period_minutes
        );
        Ok(Self {
            timestamp,
            end,
            label,
            scope: Arc::new(WindowScope::new(&market_tokens)),
            market_tokens,
            tasks: WindowTaskSet::new(),
            wind_down_started: false,
            user_stream_failed: false,
            started_at: Instant::now(),
        })
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn market_tokens(&self) -> &HashMap<B256, (U256, U256)> {
        &self.market_tokens
    }

    pub fn scope(&self) -> Arc<WindowScope> {
        self.scope.clone()
    }

    pub fn remaining_seconds(&self, now: DateTime<Utc>) -> u32 {
        (self.end - now).num_seconds().max(0) as u32
    }

    pub fn has_rolled_over(&self, now: DateTime<Utc>, period_minutes: u64) -> bool {
        let secs = window_seconds(period_minutes);
        (now.timestamp() / secs) * secs != self.timestamp
    }

    pub fn start_wind_down_if_due(&mut self, now: DateTime<Utc>, before_minutes: u64) -> bool {
        if before_minutes == 0 || self.wind_down_started {
            return false;
        }
        if (self.end - now).num_seconds() > before_minutes as i64 * 60 {
            return false;
        }
        self.wind_down_started = true;
        self.scope.pause_trading();
        true
    }

    pub fn spawn_trade(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn_trade(future);
    }

    pub fn start_wind_down(&mut self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        self.tasks.start_wind_down(future)
    }

    pub fn mark_user_stream_failed(&mut self) {
        self.user_stream_failed = true;
        self.scope.pause_trading();
    }

    #[allow(dead_code)]
    pub fn user_stream_failed(&self) -> bool {
        self.user_stream_failed
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub async fn finish(&mut self, timeout: Duration) {
        self.scope.pause_trading();
        self.tasks.finish(timeout).await;
    }
}

impl WindowTaskSet {
    pub fn new() -> Self {
        Self {
            trades: JoinSet::new(),
            wind_down: None,
        }
    }

    pub fn spawn_trade(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.trades.spawn(future);
    }

    pub fn start_wind_down(&mut self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        if self.wind_down.is_some() {
            return false;
        }
        self.wind_down = Some(tokio::spawn(future));
        true
    }

    pub async fn finish(&mut self, trade_timeout: Duration) {
        let drain_trades = async {
            while let Some(result) = self.trades.join_next().await {
                if let Err(e) = result {
                    warn!(error = %e, "Trade task failed to join");
                }
            }
        };
        if tokio::time::timeout(trade_timeout, drain_trades)
            .await
            .is_err()
        {
            warn!("Trade tasks still running after timeout; detaching without cancellation");
            self.trades.detach_all();
        }
        if let Some(task) = self.wind_down.take() {
            if let Err(e) = task.await {
                warn!(error = %e, "Wind-down task failed to join");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn market() -> MarketInfo {
        MarketInfo {
            market_id: B256::repeat_byte(1),
            slug: "btc-updown-5m-1720000000".to_string(),
            yes_token_id: U256::from(10),
            no_token_id: U256::from(11),
            title: "BTC Up or Down".to_string(),
            end_date: DateTime::from_timestamp(1_720_000_300, 0).unwrap(),
            crypto_symbol: "BTC".to_string(),
            neg_risk: false,
            outcome_prices: Some((dec!(0.5), dec!(0.5))),
        }
    }

    #[tokio::test]
    async fn finishes_trade_and_wind_down_tasks() {
        let trade_done = Arc::new(AtomicBool::new(false));
        let wind_down_done = Arc::new(AtomicBool::new(false));
        let mut tasks = WindowTaskSet::new();
        let trade_flag = trade_done.clone();
        tasks.spawn_trade(async move { trade_flag.store(true, Ordering::Release) });
        let wind_down_flag = wind_down_done.clone();
        assert!(tasks.start_wind_down(async move {
            wind_down_flag.store(true, Ordering::Release);
        }));
        assert!(!tasks.start_wind_down(async {}));
        tasks.finish(Duration::from_secs(1)).await;
        assert!(trade_done.load(Ordering::Acquire));
        assert!(wind_down_done.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn session_owns_window_state_and_pauses_at_wind_down() {
        let now = DateTime::from_timestamp(1_720_000_000, 0).unwrap();
        let mut session = WindowSession::new(&[market()], now, 5).unwrap();
        assert_eq!(session.label(), "btc-updown-5m");
        assert_eq!(session.market_tokens().len(), 1);
        assert!(!session.start_wind_down_if_due(now, 1));

        let near_end =
            DateTime::from_timestamp(session.timestamp() + window_seconds(5) - 30, 0).unwrap();
        assert!(session.start_wind_down_if_due(near_end, 1));
        assert!(session.scope().trading_paused());
        assert!(!session.start_wind_down_if_due(near_end, 1));

        session.mark_user_stream_failed();
        assert!(session.user_stream_failed());
        session.finish(Duration::from_secs(1)).await;
    }
}
