use chrono::{DateTime, Utc};
use rand::Rng;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::control::StrategyDisplayState;

const SPARKLINE_LEN: usize = 36;
const EVENT_COUNT: usize = 30;

fn timestamp_event(message: impl Into<String>) -> String {
    format!("[{}] {}", Utc::now().format("%H:%M:%S"), message.into())
}

fn push_sparkline(buf: &mut Vec<u64>, val: u64) {
    if buf.len() >= SPARKLINE_LEN {
        buf.remove(0);
    }
    buf.push(val.max(1));
}

fn random_sparkline(rng: &mut impl Rng, len: usize, start: u64, drift: i64) -> Vec<u64> {
    let mut out = Vec::with_capacity(len);
    let mut cur = start;
    for _ in 0..len {
        cur = ((cur as i64) + rng.gen_range(-1..=2) + drift).clamp(1, 20) as u64;
        out.push(cur);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PriceDir {
    Up,
    Down,
    Flat,
}

impl PriceDir {
    pub fn arrow(self) -> &'static str {
        match self {
            PriceDir::Up => "↑",
            PriceDir::Down => "↓",
            PriceDir::Flat => "−",
        }
    }

    pub fn from_delta(delta: f64) -> Self {
        if delta > 0.0005 {
            PriceDir::Up
        } else if delta < -0.0005 {
            PriceDir::Down
        } else {
            PriceDir::Flat
        }
    }
}

/// Seconds without an orderbook update after which a market row is treated as stale.
pub const MARKET_PRICE_STALE_SECS: u64 = 90;

#[derive(Clone)]
pub struct MarketRow {
    pub symbol: String,
    pub yes_price: f64,
    pub no_price: f64,
    pub yes_dir: PriceDir,
    pub no_dir: PriceDir,
    pub is_arb: bool,
    pub sparkline: Vec<u64>,
    /// When the last (partial or full) orderbook update arrived; None = never.
    pub last_update: Option<Instant>,
}

impl MarketRow {
    /// Whether the last orderbook update for this market is still fresh.
    pub fn market_fresh(&self) -> bool {
        self.last_update
            .is_some_and(|t| t.elapsed().as_secs() < MARKET_PRICE_STALE_SECS)
    }

    /// Best ask to display for the YES side, if it exists and is fresh.
    pub fn yes_display(&self) -> Option<f64> {
        if self.market_fresh() && self.yes_price > 0.0 {
            Some(self.yes_price)
        } else {
            None
        }
    }

    /// Best ask to display for the NO side, if it exists and is fresh.
    pub fn no_display(&self) -> Option<f64> {
        if self.market_fresh() && self.no_price > 0.0 {
            Some(self.no_price)
        } else {
            None
        }
    }

    /// Both sides available and fresh — used for the Σ and EDGE columns.
    pub fn both_prices(&self) -> Option<(f64, f64)> {
        match (self.yes_display(), self.no_display()) {
            (Some(yes), Some(no)) => Some((yes, no)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Ok,
    Warn,
    Err,
}

impl HealthStatus {
    pub fn dot(self) -> &'static str {
        match self {
            HealthStatus::Ok => "●",
            HealthStatus::Warn => "◐",
            HealthStatus::Err => "○",
        }
    }
}

#[derive(Clone)]
pub struct ServiceHealth {
    pub name: &'static str,
    pub status: HealthStatus,
    pub latency_ms: u32,
}

pub struct DashboardState {
    pub started_at: Instant,
    pub frame: u64,
    pub live_mode: bool,
    pub markets: Vec<MarketRow>,
    pub selected_market: usize,
    pub bid_depth: f64,
    pub ask_depth: f64,
    pub spread: f64,
    pub depth_k: f64,
    pub last_trade_secs: f32,
    pub exposure: f64,
    pub exposure_limit: f64,
    pub positions: u32,
    pub arb_scans: u64,
    pub window_secs_left: u32,
    pub window_label: String,
    pub order_mode: String,
    pub events: Vec<String>,
    pub services: Vec<ServiceHealth>,
    pub merge_status: String,
    pub flash_arb: bool,
    /// Hero metrics — profit is the star of the show.
    pub session_pnl: f64,
    pub window_pnl: f64,
    pub last_trade_pnl: f64,
    pub total_trades: u32,
    pub successful_trades: u32,
    pub best_trade: f64,
    /// Window-end late-resolution-winner-trading (strategy B) stats.
    pub late_resolution_winner_trading_fills: u64,
    pub late_resolution_winner_trading_scans: u64,
    pub late_resolution_winner_trading_signals: u64,
    pub late_resolution_winner_trading_attempts: u64,
    pub late_resolution_winner_trading_model_rejections: u64,
    pub late_resolution_winner_trading_awaiting_confirmation: u64,
    pub late_resolution_winner_trading_pending: u64,
    pub late_resolution_winner_trading_resolved: u64,
    pub late_resolution_winner_trading_redeemed: u64,
    pub late_resolution_winner_trading_wins: u64,
    pub late_resolution_winner_trading_losses: u64,
    pub late_resolution_winner_trading_est_pnl: f64,
    pub late_resolution_winner_trading_realized_pnl: f64,
    pub pnl_sparkline: Vec<u64>,
    pub window_pnl_sparkline: Vec<u64>,
    pub exposure_sparkline: Vec<u64>,
    pub edge_sparkline: Vec<u64>,
    pub scan_rate_sparkline: Vec<u64>,
    /// Frames remaining for profit pulse animation after a win.
    pub profit_pulse: u32,
    pub last_trade_symbol: String,
    pub connected: bool,
    pub execution_mode: String,
    pub selected_strategy: usize,
    pub binary_complement_arbitrage_state: StrategyDisplayState,
    pub late_resolution_winner_trading_state: StrategyDisplayState,
    pub wallet_copy_trading_state: StrategyDisplayState,
    pub strategy_message: String,
    pub binary_complement_arbitrage_budget: f64,
    pub late_resolution_winner_trading_budget: f64,
    pub late_resolution_winner_trading_exposure: f64,
    pub wallet_copy_trading_budget: f64,
    pub copy_whitelist_wallets: u64,
    pub copy_signals_seen: u64,
    pub copy_signals_rejected: u64,
    pub copy_proposals: u64,
    pub copy_fills: u64,
    pub copy_open_positions: u64,
    pub copy_nav: f64,
    pub copy_cash: f64,
    pub copy_last_poll_at_ms: i64,
    pub copy_last_signal_at_ms: i64,
    pub opportunities_admitted: u64,
    pub opportunities_rejected: u64,
    pub depth_rejections: u64,
    pub risk_rejections: u64,
    pub quote_rejections: u64,
    pub trades_started: u64,
    pub trades_succeeded: u64,
    pub trades_failed: u64,
    pub paper_one_sided: u64,
    pub paper_pnl: f64,
    pub winner_data_rejections: u64,
    pub winner_market_rejections: u64,
    pub winner_boundary_rejections: u64,
    pub winner_ev_rejections: u64,
    pub winner_shadow_ev_failures: u64,
}

impl DashboardState {
    pub fn new_live(order_mode: impl Into<String>, exposure_limit: f64) -> Self {
        Self {
            started_at: Instant::now(),
            frame: 0,
            live_mode: true,
            markets: Vec::new(),
            selected_market: 0,
            bid_depth: 0.5,
            ask_depth: 0.5,
            spread: 0.0,
            depth_k: 0.0,
            last_trade_secs: 999.0,
            exposure: 0.0,
            exposure_limit,
            positions: 0,
            arb_scans: 0,
            window_secs_left: 300,
            window_label: "updown-5m".to_string(),
            order_mode: order_mode.into(),
            events: vec![timestamp_event("🚀 Bot started — scanning for arbitrage…")],
            services: vec![
                ServiceHealth {
                    name: "MARKET WS",
                    status: HealthStatus::Warn,
                    latency_ms: 0,
                },
                ServiceHealth {
                    name: "USER WS",
                    status: HealthStatus::Warn,
                    latency_ms: 0,
                },
                ServiceHealth {
                    name: "RTDS",
                    status: HealthStatus::Warn,
                    latency_ms: 0,
                },
                ServiceHealth {
                    name: "SAFETY",
                    status: HealthStatus::Warn,
                    latency_ms: 0,
                },
            ],
            merge_status: "idle".to_string(),
            flash_arb: false,
            session_pnl: 0.0,
            window_pnl: 0.0,
            last_trade_pnl: 0.0,
            total_trades: 0,
            successful_trades: 0,
            best_trade: 0.0,
            late_resolution_winner_trading_fills: 0,
            late_resolution_winner_trading_scans: 0,
            late_resolution_winner_trading_signals: 0,
            late_resolution_winner_trading_attempts: 0,
            late_resolution_winner_trading_model_rejections: 0,
            late_resolution_winner_trading_awaiting_confirmation: 0,
            late_resolution_winner_trading_pending: 0,
            late_resolution_winner_trading_resolved: 0,
            late_resolution_winner_trading_redeemed: 0,
            late_resolution_winner_trading_wins: 0,
            late_resolution_winner_trading_losses: 0,
            late_resolution_winner_trading_est_pnl: 0.0,
            late_resolution_winner_trading_realized_pnl: 0.0,
            pnl_sparkline: vec![2, 3, 3, 4, 5, 6, 5, 7, 8, 9, 10, 11],
            window_pnl_sparkline: vec![1, 2, 2, 3, 4, 5, 4, 6, 7, 8],
            exposure_sparkline: vec![3, 4, 5, 4, 6, 5, 7, 6, 8, 7],
            edge_sparkline: vec![2, 4, 3, 5, 6, 7, 5, 8, 9, 7],
            scan_rate_sparkline: vec![4, 5, 6, 5, 7, 8, 7, 9, 8, 10],
            profit_pulse: 0,
            last_trade_symbol: String::new(),
            connected: false,
            execution_mode: "LIVE".into(),
            selected_strategy: 0,
            binary_complement_arbitrage_state: StrategyDisplayState::Blocked,
            late_resolution_winner_trading_state: StrategyDisplayState::Unavailable,
            wallet_copy_trading_state: StrategyDisplayState::Unavailable,
            strategy_message: "initializing".into(),
            binary_complement_arbitrage_budget: exposure_limit,
            late_resolution_winner_trading_budget: 0.0,
            late_resolution_winner_trading_exposure: 0.0,
            wallet_copy_trading_budget: 0.0,
            copy_whitelist_wallets: 0,
            copy_signals_seen: 0,
            copy_signals_rejected: 0,
            copy_proposals: 0,
            copy_fills: 0,
            copy_open_positions: 0,
            copy_nav: 0.0,
            copy_cash: 0.0,
            copy_last_poll_at_ms: 0,
            copy_last_signal_at_ms: 0,
            opportunities_admitted: 0,
            opportunities_rejected: 0,
            depth_rejections: 0,
            risk_rejections: 0,
            quote_rejections: 0,
            trades_started: 0,
            trades_succeeded: 0,
            trades_failed: 0,
            paper_one_sided: 0,
            paper_pnl: 0.0,
            winner_data_rejections: 0,
            winner_market_rejections: 0,
            winner_boundary_rejections: 0,
            winner_ev_rejections: 0,
            winner_shadow_ev_failures: 0,
        }
    }

    pub fn new_demo() -> Self {
        let mut rng = rand::thread_rng();

        // Pretend the bot has been running for 25–175 minutes.
        let elapsed_secs = rng.gen_range(25 * 60..175 * 60);
        let session_pnl = rng.gen_range(48.0..286.0);
        let window_ratio = rng.gen_range(0.12..0.38);
        let window_pnl = session_pnl * window_ratio;
        let total_trades = rng.gen_range(18..76);
        let win_ratio = rng.gen_range(0.58..0.82);
        let successful_trades = ((total_trades as f64) * win_ratio).round() as u32;
        let best_trade = rng.gen_range(3.5..14.0);
        let last_trade_pnl = rng.gen_range(1.2..best_trade);
        let exposure = rng.gen_range(180.0..780.0);
        let arb_scans = rng.gen_range(900..5200);
        let window_secs_left = rng.gen_range(45..260);
        let last_trade_secs = rng.gen_range(1.5..18.0);
        let symbols = ["BTC", "ETH", "SOL", "XRP"];
        let last_sym = symbols[rng.gen_range(0..symbols.len())];

        let mut s = Self::new_live("GTD/FAK", 1000.0);
        s.live_mode = false;
        s.started_at = Instant::now() - Duration::from_secs(elapsed_secs);
        s.frame = rng.gen_range(200..1200);
        s.session_pnl = session_pnl;
        s.window_pnl = window_pnl;
        s.last_trade_pnl = last_trade_pnl;
        s.total_trades = total_trades;
        s.successful_trades = successful_trades.min(total_trades);
        s.best_trade = best_trade;
        s.last_trade_secs = last_trade_secs;
        s.last_trade_symbol = last_sym.to_string();
        s.connected = true;
        s.exposure = exposure;
        s.arb_scans = arb_scans;
        s.window_secs_left = window_secs_left;
        s.window_label = format!("{}-updown-5m", last_sym.to_lowercase());

        let pnl_end = ((session_pnl / 4.0).clamp(2.0, 20.0)) as u64;
        s.pnl_sparkline = random_sparkline(&mut rng, SPARKLINE_LEN, pnl_end.saturating_sub(8), 1);
        s.window_pnl_sparkline = random_sparkline(&mut rng, SPARKLINE_LEN, (pnl_end / 2).max(2), 1);
        s.exposure_sparkline = random_sparkline(
            &mut rng,
            SPARKLINE_LEN,
            ((exposure / 1000.0) * 16.0) as u64 + 2,
            0,
        );
        let edge_start = rng.gen_range(4..9);
        let scan_start = rng.gen_range(5..10);
        s.edge_sparkline = random_sparkline(&mut rng, SPARKLINE_LEN, edge_start, 0);
        s.scan_rate_sparkline = random_sparkline(&mut rng, SPARKLINE_LEN, scan_start, 0);

        s.markets = vec![
            MarketRow {
                symbol: "BTC".into(),
                yes_price: rng.gen_range(0.44..0.52),
                no_price: rng.gen_range(0.46..0.54),
                yes_dir: PriceDir::Up,
                no_dir: PriceDir::Down,
                is_arb: true,
                sparkline: random_sparkline(&mut rng, SPARKLINE_LEN / 2, 6, 1),
                last_update: Some(Instant::now()),
            },
            MarketRow {
                symbol: "ETH".into(),
                yes_price: rng.gen_range(0.46..0.54),
                no_price: rng.gen_range(0.44..0.52),
                yes_dir: PriceDir::Flat,
                no_dir: PriceDir::Up,
                is_arb: false,
                sparkline: random_sparkline(&mut rng, SPARKLINE_LEN / 2, 4, 0),
                last_update: Some(Instant::now()),
            },
            MarketRow {
                symbol: "SOL".into(),
                yes_price: rng.gen_range(0.42..0.50),
                no_price: rng.gen_range(0.48..0.56),
                yes_dir: PriceDir::Down,
                no_dir: PriceDir::Up,
                is_arb: true,
                sparkline: random_sparkline(&mut rng, SPARKLINE_LEN / 2, 7, 1),
                last_update: Some(Instant::now()),
            },
            MarketRow {
                symbol: "XRP".into(),
                yes_price: rng.gen_range(0.47..0.53),
                no_price: rng.gen_range(0.46..0.52),
                yes_dir: PriceDir::Up,
                no_dir: PriceDir::Flat,
                is_arb: false,
                sparkline: random_sparkline(&mut rng, SPARKLINE_LEN / 2, 4, 0),
                last_update: Some(Instant::now()),
            },
        ];

        s.events = Self::demo_event_history(
            &mut rng,
            session_pnl,
            window_pnl,
            last_sym,
            last_trade_pnl,
            successful_trades,
        )
        .into_iter()
        .map(timestamp_event)
        .collect();

        // Fast-forward 60–180 ticks so curves & prices look mid-session, not freshly booted.
        let warmup_ticks = rng.gen_range(60..180);
        s.warmup_demo(warmup_ticks);

        s
    }

    fn demo_event_history(
        rng: &mut impl Rng,
        session_pnl: f64,
        window_pnl: f64,
        last_sym: &str,
        last_trade_pnl: f64,
        wins: u32,
    ) -> Vec<String> {
        let symbols = ["BTC", "ETH", "SOL", "XRP"];
        let mut events = vec![
            format!("🚀 Session started — running for a while already"),
            format!("📡 Subscribed 8 orderbook tokens (4 markets)"),
            format!("💰 Window PnL +${window_pnl:.2} | session +${session_pnl:.2}"),
        ];
        for _ in 0..rng.gen_range(2..5) {
            let sym = symbols[rng.gen_range(0..symbols.len())];
            let p = rng.gen_range(0.8..6.5);
            events.push(format!("💰 +${p:.2} captured on {sym}"));
        }
        events.push(format!(
            "⚡ ARB {last_sym} +${last_trade_pnl:.2} | {wins} wins so far"
        ));
        events.push("📊 Spread stable — scanning next window".to_string());
        events
    }

    /// Silently simulate market motion before the dashboard is shown.
    fn warmup_demo(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.frame = self.frame.wrapping_add(1);
            if self.frame.is_multiple_of(2) {
                self.tick_sparklines();
            }
            self.on_demo_tick(false);
        }
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            0.0
        } else {
            (self.successful_trades as f64 / self.total_trades as f64) * 100.0
        }
    }

    pub fn uptime(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    pub fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    pub fn window_countdown(&self) -> String {
        let m = self.window_secs_left / 60;
        let s = self.window_secs_left % 60;
        format!("{m:02}:{s:02}")
    }

    pub fn exposure_pct(&self) -> f64 {
        if self.exposure_limit <= 0.0 {
            0.0
        } else {
            (self.exposure / self.exposure_limit) * 100.0
        }
    }

    pub fn profit_pct(&self, market: &MarketRow) -> f64 {
        let Some((yes, no)) = market.both_prices() else {
            return 0.0;
        };
        let t = yes + no;
        if t < 1.0 {
            (1.0 - t) * 100.0
        } else {
            0.0
        }
    }

    /// Return the most recent events for the log panel (oldest first).
    pub fn recent_events(&self, count: usize) -> Vec<String> {
        if self.events.is_empty() {
            return vec!["Waiting for events…".to_string()];
        }
        let take = count.min(self.events.len());
        self.events[self.events.len() - take..].to_vec()
    }

    pub fn push_event(&mut self, msg: impl Into<String>) {
        let msg = timestamp_event(msg);
        if self.events.len() >= EVENT_COUNT {
            self.events.remove(0);
        }
        self.events.push(msg);
    }

    pub fn set_window(&mut self, label: impl Into<String>, secs_left: u32) {
        self.window_label = label.into();
        self.window_secs_left = secs_left;
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    pub fn set_exposure(&mut self, exposure: f64) {
        self.exposure = exposure;
    }

    pub fn set_merge_status(&mut self, status: impl Into<String>) {
        self.merge_status = status.into();
    }

    pub fn ensure_market(&mut self, symbol: impl Into<String>) {
        let symbol = symbol.into();
        if !self.markets.iter().any(|m| m.symbol == symbol) {
            self.markets.push(MarketRow {
                symbol,
                yes_price: 0.0,
                no_price: 0.0,
                yes_dir: PriceDir::Flat,
                no_dir: PriceDir::Flat,
                is_arb: false,
                sparkline: vec![1],
                last_update: None,
            });
        }
    }

    /// Update one or both sides of a market's displayed best-ask price.
    /// `None` means that side has no best ask (no liquidity) and the TUI shows "—".
    pub fn update_market(&mut self, symbol: &str, yes: Option<f64>, no: Option<f64>, is_arb: bool) {
        self.ensure_market(symbol);
        if let Some(row) = self.markets.iter_mut().find(|m| m.symbol == symbol) {
            if let Some(yes) = yes {
                row.yes_dir = PriceDir::from_delta(yes - row.yes_price);
                row.yes_price = yes;
            }
            if let Some(no) = no {
                row.no_dir = PriceDir::from_delta(no - row.no_price);
                row.no_price = no;
            }
            if let (Some(yes), Some(no)) = (yes, no) {
                let edge = ((1.0 - yes - no).max(0.0) * 100.0 * 10.0) as u64;
                push_sparkline(&mut row.sparkline, edge);
            }
            row.is_arb = is_arb;
            row.last_update = Some(Instant::now());
        }
        self.arb_scans = self.arb_scans.saturating_add(1);
        self.connected = true;
        if let (Some(yes), Some(no)) = (yes, no) {
            self.spread = (yes - no).abs();
            self.bid_depth = 0.4 + (yes * 0.3);
            self.ask_depth = 0.4 + (no * 0.3);
            self.depth_k = yes + no;
        }
    }

    pub fn record_trade_attempt(&mut self, symbol: &str, profit_pct: f64, size: f64, cost: f64) {
        self.total_trades = self.total_trades.saturating_add(1);
        self.last_trade_symbol = symbol.to_string();
        self.push_event(format!(
            "⚡ Executing {symbol} | edge +{profit_pct:.2}% | ${cost:.2}"
        ));
        let _ = size;
    }

    pub fn record_trade_success(&mut self, symbol: &str, profit_usd: f64, profit_pct: f64) {
        self.successful_trades = self.successful_trades.saturating_add(1);
        self.session_pnl += profit_usd;
        self.window_pnl += profit_usd;
        self.last_trade_pnl = profit_usd;
        self.last_trade_symbol = symbol.to_string();
        self.last_trade_secs = 0.0;
        if profit_usd > self.best_trade {
            self.best_trade = profit_usd;
        }
        self.profit_pulse = 15;
        let spark_val = ((self.session_pnl / 5.0).clamp(2.0, 20.0)) as u64;
        push_sparkline(&mut self.pnl_sparkline, spark_val);
        push_sparkline(
            &mut self.window_pnl_sparkline,
            ((self.window_pnl / 2.0).clamp(1.0, 20.0)) as u64,
        );
        self.push_event(format!(
            "💰 +${profit_usd:.2} captured on {symbol} (+{profit_pct:.2}% edge) | session ${:.2}",
            self.session_pnl
        ));
    }

    pub fn record_trade_failure(&mut self, symbol: &str, err: &str) {
        self.push_event(format!("❌ {symbol} failed: {err}"));
    }

    /// Strategy B: a late-resolution-winner-trading paper fill was recorded.
    pub fn record_late_resolution_winner_trading_fill(&mut self, est_pnl: f64) {
        self.late_resolution_winner_trading_fills =
            self.late_resolution_winner_trading_fills.saturating_add(1);
        self.late_resolution_winner_trading_est_pnl += est_pnl;
        self.push_event(format!(
            "🎯 Late-Resolution Winner Trading filled | est +${est_pnl:.2}"
        ));
    }

    /// Strategy B: a paper late-resolution-winner-trading position settled against the real market.
    pub fn record_late_resolution_winner_trading_settlement(&mut self, pnl: f64, won: bool) {
        if won {
            self.late_resolution_winner_trading_wins =
                self.late_resolution_winner_trading_wins.saturating_add(1);
        } else {
            self.late_resolution_winner_trading_losses =
                self.late_resolution_winner_trading_losses.saturating_add(1);
        }
        self.late_resolution_winner_trading_realized_pnl += pnl;
        let icon = if won { "✅" } else { "❌" };
        self.push_event(format!(
            "🏁 Late-Resolution Winner Trading settled {icon} ${pnl:.2}"
        ));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_late_resolution_winner_trading_funnel(
        &mut self,
        scans: u64,
        signals: u64,
        attempts: u64,
        model_rejections: u64,
        awaiting_confirmation: u64,
        pending: u64,
        resolved: u64,
        redeemed: u64,
    ) {
        self.late_resolution_winner_trading_scans = scans;
        self.late_resolution_winner_trading_signals = signals;
        self.late_resolution_winner_trading_attempts = attempts;
        self.late_resolution_winner_trading_model_rejections = model_rejections;
        self.late_resolution_winner_trading_awaiting_confirmation = awaiting_confirmation;
        self.late_resolution_winner_trading_pending = pending;
        self.late_resolution_winner_trading_resolved = resolved;
        self.late_resolution_winner_trading_redeemed = redeemed;
    }

    pub fn set_execution_mode(&mut self, mode: impl Into<String>) {
        self.execution_mode = mode.into();
    }

    pub fn set_strategy_states(
        &mut self,
        arbitrage: StrategyDisplayState,
        late_resolution_winner_trading: StrategyDisplayState,
        wallet_copy_trading: StrategyDisplayState,
        message: impl Into<String>,
    ) {
        self.binary_complement_arbitrage_state = arbitrage;
        self.late_resolution_winner_trading_state = late_resolution_winner_trading;
        self.wallet_copy_trading_state = wallet_copy_trading;
        self.strategy_message = message.into();
    }

    pub fn set_strategy_budgets(
        &mut self,
        arbitrage: f64,
        late_resolution_winner_trading: f64,
        copy: f64,
    ) {
        self.binary_complement_arbitrage_budget = arbitrage;
        self.late_resolution_winner_trading_budget = late_resolution_winner_trading;
        self.wallet_copy_trading_budget = copy;
    }

    pub fn set_late_resolution_winner_trading_exposure(&mut self, exposure: f64) {
        self.late_resolution_winner_trading_exposure = exposure;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_wallet_copy_trading_metrics(
        &mut self,
        whitelist_wallets: u64,
        signals_seen: u64,
        signals_rejected: u64,
        proposals: u64,
        fills: u64,
        open_positions: u64,
        nav: f64,
        cash: f64,
        last_poll_at_ms: i64,
        last_signal_at_ms: i64,
    ) {
        self.copy_whitelist_wallets = whitelist_wallets;
        self.copy_signals_seen = signals_seen;
        self.copy_signals_rejected = signals_rejected;
        self.copy_proposals = proposals;
        self.copy_fills = fills;
        self.copy_open_positions = open_positions;
        self.copy_nav = nav;
        self.copy_cash = cash;
        self.copy_last_poll_at_ms = last_poll_at_ms;
        self.copy_last_signal_at_ms = last_signal_at_ms;
    }

    pub fn set_runtime_health(&mut self, market_ws: bool, user_ws: bool, rtds: bool, safety: bool) {
        for (name, healthy) in [
            ("MARKET WS", market_ws),
            ("USER WS", user_ws),
            ("RTDS", rtds),
            ("SAFETY", safety),
        ] {
            if let Some(service) = self
                .services
                .iter_mut()
                .find(|service| service.name == name)
            {
                service.status = if healthy {
                    HealthStatus::Ok
                } else {
                    HealthStatus::Err
                };
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_strategy_metrics(
        &mut self,
        opportunities_admitted: u64,
        opportunities_rejected: u64,
        depth_rejections: u64,
        risk_rejections: u64,
        quote_rejections: u64,
        trades_started: u64,
        trades_succeeded: u64,
        trades_failed: u64,
        paper_one_sided: u64,
        paper_pnl: f64,
        winner_data_rejections: u64,
        winner_market_rejections: u64,
        winner_boundary_rejections: u64,
        winner_ev_rejections: u64,
        winner_shadow_ev_failures: u64,
    ) {
        self.opportunities_admitted = opportunities_admitted;
        self.opportunities_rejected = opportunities_rejected;
        self.depth_rejections = depth_rejections;
        self.risk_rejections = risk_rejections;
        self.quote_rejections = quote_rejections;
        self.trades_started = trades_started;
        self.trades_succeeded = trades_succeeded;
        self.trades_failed = trades_failed;
        self.paper_one_sided = paper_one_sided;
        self.paper_pnl = paper_pnl;
        self.winner_data_rejections = winner_data_rejections;
        self.winner_market_rejections = winner_market_rejections;
        self.winner_boundary_rejections = winner_boundary_rejections;
        self.winner_ev_rejections = winner_ev_rejections;
        self.winner_shadow_ev_failures = winner_shadow_ev_failures;
    }

    pub fn on_render_tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);

        if self.last_trade_secs < 999.0 {
            self.last_trade_secs += 0.2;
        }

        self.flash_arb = self.frame % 16 < 4;
        if self.profit_pulse > 0 {
            self.profit_pulse -= 1;
        }

        if self.frame.is_multiple_of(2) {
            self.tick_sparklines();
        }

        if !self.live_mode {
            self.on_demo_tick(true);
        }
    }

    fn tick_sparklines(&mut self) {
        let pnl_val = ((self.session_pnl / 4.0).clamp(1.0, 20.0)) as u64;
        push_sparkline(&mut self.pnl_sparkline, pnl_val);

        let window_val = ((self.window_pnl / 2.0).clamp(1.0, 20.0)) as u64;
        push_sparkline(&mut self.window_pnl_sparkline, window_val);

        let exp_pct = if self.exposure_limit > 0.0 {
            self.exposure / self.exposure_limit
        } else {
            0.0
        };
        let exp_val = (exp_pct * 18.0).round() as u64 + 2;
        push_sparkline(&mut self.exposure_sparkline, exp_val);

        let avg_edge = if self.markets.is_empty() {
            2
        } else {
            let sum: f64 = self.markets.iter().map(|m| self.profit_pct(m)).sum();
            ((sum / self.markets.len() as f64) * 4.0).clamp(1.0, 20.0) as u64
        };
        push_sparkline(&mut self.edge_sparkline, avg_edge);

        let breath = 6 + ((self.frame as f64 * 0.12).sin() * 4.0).round() as u64;
        let scan_val = breath.saturating_add(self.arb_scans % 5);
        push_sparkline(&mut self.scan_rate_sparkline, scan_val);
    }

    fn on_demo_tick(&mut self, grow_pnl: bool) {
        let mut rng = rand::thread_rng();
        if grow_pnl && self.frame.is_multiple_of(5) && self.window_secs_left > 0 {
            self.window_secs_left = self.window_secs_left.saturating_sub(1);
        }
        if grow_pnl {
            self.session_pnl += rng.gen_range(0.0..0.15);
            self.window_pnl += rng.gen_range(0.0..0.08);
            self.arb_scans = self.arb_scans.wrapping_add(rng.gen_range(1..=4));
        }
        for market in &mut self.markets {
            market.last_update = Some(Instant::now());
            let j: f64 = rng.gen_range(-0.002..0.002);
            market.yes_price = (market.yes_price + j).clamp(0.01, 0.99);
            market.no_price = (market.no_price - j * 0.5).clamp(0.01, 0.99);
            let edge = ((1.0 - market.yes_price - market.no_price).max(0.0) * 100.0 * 10.0) as u64;
            push_sparkline(&mut market.sparkline, edge.max(1));
        }
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyCode) -> DashboardAction {
        match key {
            crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc => {
                DashboardAction::Quit
            }
            crossterm::event::KeyCode::Up
            | crossterm::event::KeyCode::Left
            | crossterm::event::KeyCode::Char('k') => {
                self.selected_strategy = self.selected_strategy.saturating_sub(1);
                DashboardAction::None
            }
            crossterm::event::KeyCode::Down
            | crossterm::event::KeyCode::Right
            | crossterm::event::KeyCode::Char('j') => {
                self.selected_strategy = (self.selected_strategy + 1).min(2);
                DashboardAction::None
            }
            crossterm::event::KeyCode::Char('1') => {
                self.selected_strategy = 0;
                DashboardAction::None
            }
            crossterm::event::KeyCode::Char('2') => {
                self.selected_strategy = 1;
                DashboardAction::None
            }
            crossterm::event::KeyCode::Char('3') => {
                self.selected_strategy = 2;
                DashboardAction::None
            }
            crossterm::event::KeyCode::Char(' ') => {
                DashboardAction::ToggleStrategy(self.selected_strategy)
            }
            crossterm::event::KeyCode::Char('p') | crossterm::event::KeyCode::Char('P') => {
                DashboardAction::PauseAll
            }
            crossterm::event::KeyCode::Char('y') | crossterm::event::KeyCode::Char('Y') => {
                DashboardAction::ConfirmEnable
            }
            _ => DashboardAction::None,
        }
    }
}

pub enum DashboardAction {
    None,
    Quit,
    ToggleStrategy(usize),
    PauseAll,
    ConfirmEnable,
}

#[derive(Clone)]
pub struct DashboardHandle {
    inner: Arc<Mutex<DashboardState>>,
}

impl DashboardHandle {
    pub fn new_live(order_mode: impl Into<String>, exposure_limit: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DashboardState::new_live(
                order_mode,
                exposure_limit,
            ))),
        }
    }

    pub fn new_demo() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DashboardState::new_demo())),
        }
    }

    pub fn arc(&self) -> Arc<Mutex<DashboardState>> {
        self.inner.clone()
    }

    pub fn with_mut<R>(&self, f: impl FnOnce(&mut DashboardState) -> R) -> R {
        let mut guard = self.inner.lock().expect("dashboard lock");
        f(&mut guard)
    }
}

pub fn symbol_short(crypto_symbol: &str) -> String {
    match crypto_symbol.to_lowercase().as_str() {
        "bitcoin" => "BTC".into(),
        "ethereum" => "ETH".into(),
        "solana" => "SOL".into(),
        "xrp" => "XRP".into(),
        other => other.chars().take(4).collect::<String>().to_uppercase(),
    }
}

pub fn decimal_to_f64(d: rust_decimal::Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_market_without_data_displays_dash() {
        let mut state = DashboardState::new_live(String::from("GTD"), 1000.0);
        state.ensure_market("BTC");
        let row = state.markets.iter().find(|m| m.symbol == "BTC").unwrap();
        // Never updated: no price available.
        assert_eq!(row.yes_display(), None);
        assert_eq!(row.no_display(), None);
        assert_eq!(row.both_prices(), None);
    }

    #[test]
    fn one_sided_update_shows_only_the_available_side() {
        let mut state = DashboardState::new_live(String::from("GTD"), 1000.0);
        state.update_market("BTC", Some(0.99), None, false);
        let row = state.markets.iter().find(|m| m.symbol == "BTC").unwrap();
        assert_eq!(row.yes_display(), Some(0.99));
        assert_eq!(row.no_display(), None);
        assert_eq!(row.both_prices(), None);
        assert_eq!(state.profit_pct(row), 0.0);
    }

    #[test]
    fn two_sided_update_enables_total_and_edge() {
        let mut state = DashboardState::new_live(String::from("GTD"), 1000.0);
        state.update_market("BTC", Some(0.52), Some(0.47), true);
        let row = state.markets.iter().find(|m| m.symbol == "BTC").unwrap();
        assert_eq!(row.yes_display(), Some(0.52));
        assert_eq!(row.no_display(), Some(0.47));
        assert_eq!(row.both_prices(), Some((0.52, 0.47)));
        assert!((state.profit_pct(row) - 1.0).abs() < 1e-9);
        assert!(row.is_arb);
    }

    #[test]
    fn event_history_prefixes_each_entry_with_utc_time() {
        let mut state = DashboardState::new_live(String::from("GTD"), 1000.0);
        state.push_event("test event");
        let latest = state.recent_events(1).pop().unwrap();
        assert!(latest.starts_with('['));
        assert_eq!(latest.as_bytes().get(3), Some(&b':'));
        assert_eq!(latest.as_bytes().get(6), Some(&b':'));
        assert!(latest.contains("] test event"));
    }

    #[test]
    fn stale_market_shows_dash_after_threshold() {
        let mut state = DashboardState::new_live(String::from("GTD"), 1000.0);
        state.update_market("BTC", Some(0.60), Some(0.40), false);
        let row = state
            .markets
            .iter_mut()
            .find(|m| m.symbol == "BTC")
            .unwrap();
        // Simulate an old update: force last_update far in the past.
        row.last_update = Some(Instant::now() - Duration::from_secs(MARKET_PRICE_STALE_SECS + 5));
        assert!(!row.market_fresh());
        assert_eq!(row.yes_display(), None);
        assert_eq!(row.no_display(), None);
    }

    #[test]
    fn strategy_keyboard_navigation_emits_control_actions() {
        let mut state = DashboardState::new_live("FAK", 1000.0);
        assert_eq!(state.selected_strategy, 0);
        assert!(matches!(
            state.on_key(crossterm::event::KeyCode::Char(' ')),
            DashboardAction::ToggleStrategy(0)
        ));
        state.on_key(crossterm::event::KeyCode::Down);
        assert_eq!(state.selected_strategy, 1);
        state.on_key(crossterm::event::KeyCode::Left);
        assert_eq!(state.selected_strategy, 0);
        state.on_key(crossterm::event::KeyCode::Right);
        assert_eq!(state.selected_strategy, 1);
        assert!(matches!(
            state.on_key(crossterm::event::KeyCode::Char(' ')),
            DashboardAction::ToggleStrategy(1)
        ));
        state.on_key(crossterm::event::KeyCode::Right);
        assert_eq!(state.selected_strategy, 2);
        assert!(matches!(
            state.on_key(crossterm::event::KeyCode::Char(' ')),
            DashboardAction::ToggleStrategy(2)
        ));
        assert!(matches!(
            state.on_key(crossterm::event::KeyCode::Char('p')),
            DashboardAction::PauseAll
        ));
    }
}
