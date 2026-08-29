use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Domain classification tags for prediction markets and wallet expertise.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainTag {
    MacroInterestRate,
    Geopolitics,
    TechRegulation,
    Crypto,
    Elections,
    ScienceClimate,
    PopCultureSports,
    General,
    Custom(String),
}

impl fmt::Display for DomainTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainTag::MacroInterestRate => write!(f, "Macro/InterestRate"),
            DomainTag::Geopolitics => write!(f, "Geopolitics"),
            DomainTag::TechRegulation => write!(f, "Tech/Regulation"),
            DomainTag::Crypto => write!(f, "Crypto"),
            DomainTag::Elections => write!(f, "Elections"),
            DomainTag::ScienceClimate => write!(f, "Science/Climate"),
            DomainTag::PopCultureSports => write!(f, "PopCulture/Sports"),
            DomainTag::General => write!(f, "General"),
            DomainTag::Custom(name) => write!(f, "Custom({})", name),
        }
    }
}

/// Directional outcome side on prediction markets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutcomeSide {
    Yes,
    No,
    Other(String),
}

impl fmt::Display for OutcomeSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutcomeSide::Yes => write!(f, "YES"),
            OutcomeSide::No => write!(f, "NO"),
            OutcomeSide::Other(s) => write!(f, "{}", s),
        }
    }
}

/// Wallet classification tier based on historical performance and calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalletTier {
    /// S-Tier: Elite forecasters with low Brier score & sustained track record. Single signal can trigger.
    TierS,
    /// A-Tier: Qualified experts meeting all funnel criteria. Requires consensus.
    TierA,
    /// Probation: Degraded due to performance decay or warning flags.
    Probation,
    /// Blacklisted: Hard-filtered (Maker > 90%, arbitrageur, single-profit survivor, or 3-strike failed).
    Blacklisted,
}

/// Historical settled trade record for a wallet to evaluate performance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletTradeRecord {
    pub trade_id: String,
    pub market_id: String,
    pub domain: DomainTag,
    pub outcome_bought: OutcomeSide,
    pub entry_price: Decimal,
    pub exit_price: Option<Decimal>,
    pub notional_usdc: Decimal,
    pub pnl_usdc: Decimal,
    pub return_pct: Decimal,
    pub is_maker: bool,
    pub is_arbitrage_leg: bool,
    /// Whether maker/arbitrage classifications are sourced rather than defaulted.
    #[serde(default)]
    pub behavior_data_known: bool,
    pub resolved_outcome: Option<OutcomeSide>,
    pub opened_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

/// Comprehensive wallet profile evaluated across multiple dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletProfile {
    pub address: String,
    pub tier: WalletTier,
    pub total_settled_events: usize,
    pub active_span_days: u32,
    pub avg_entry_price: Decimal,
    pub max_single_profit_ratio: Decimal,
    pub maker_volume_ratio: Decimal,
    #[serde(default)]
    pub behavior_data_coverage: Decimal,
    pub is_arbitrageur: bool,
    pub is_single_strike_survivor: bool,
    pub primary_domain: DomainTag,
    pub domain_shares: Vec<(DomainTag, Decimal)>,
    pub brier_score: Decimal,
    pub win_rate: Decimal,
    pub total_pnl_usdc: Decimal,
    pub consecutive_major_direction_errors: u32,
    pub last_evaluated_at: DateTime<Utc>,
    pub rejection_reason: Option<String>,
}

/// Market metadata for screening and filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMetadata {
    pub market_id: String,
    pub condition_id: String,
    pub question: String,
    pub domain: DomainTag,
    pub outcome_count: usize,
    pub is_neg_risk: bool,
    pub settlement_time: DateTime<Utc>,
    pub volume_24h_usdc: Decimal,
    pub is_active: bool,
}

/// Single level in the CLOB orderbook.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// Orderbook snapshot for liquidity, spread, and Top-5 depth calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub market_id: String,
    pub token_id: String,
    pub bids: Vec<BookLevel>, // Sorted descending (best bid first)
    pub asks: Vec<BookLevel>, // Sorted ascending (best ask first)
    pub timestamp: DateTime<Utc>,
}

impl OrderBookSnapshot {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > Decimal::ZERO => {
                Some((bid + ask) / dec!(2))
            }
            _ => None,
        }
    }

    pub fn relative_spread(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask(), self.mid_price()) {
            (Some(bid), Some(ask), Some(mid)) if mid > Decimal::ZERO => Some((ask - bid) / mid),
            _ => None,
        }
    }

    /// Calculates total notional liquidity in Top-N ask levels.
    pub fn top_n_ask_depth_notional(&self, n: usize) -> Decimal {
        self.asks
            .iter()
            .take(n)
            .fold(Decimal::ZERO, |acc, level| acc + (level.price * level.size))
    }

    /// Calculates total notional liquidity in Top-N bid levels.
    pub fn top_n_bid_depth_notional(&self, n: usize) -> Decimal {
        self.bids
            .iter()
            .take(n)
            .fold(Decimal::ZERO, |acc, level| acc + (level.price * level.size))
    }
}

/// Type of trade action observed from target signal source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalAction {
    OpenBuy,
    ScaleOut,
    CloseSell,
    ReversePosition,
}

/// Raw signal ingested from wallet activity (on-chain log or CLOB trade).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEvent {
    pub signal_id: String,
    pub source_wallet: String,
    pub market_id: String,
    pub token_id: String,
    pub outcome: OutcomeSide,
    pub domain: DomainTag,
    pub action: SignalAction,
    pub target_vwap: Decimal,
    pub target_shares: Decimal,
    pub target_notional_usdc: Decimal,
    /// Fraction of the source wallet's pre-trade token position that was sold.
    /// Present for observed SELL signals; None means the feed could not establish
    /// a reliable position denominator.
    #[serde(default)]
    pub target_reduction_pct: Option<Decimal>,
    pub timestamp: DateTime<Utc>,
}

/// Aggregated multi-source consensus signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusSignal {
    pub consensus_id: String,
    pub market_id: String,
    pub token_id: String,
    pub outcome: OutcomeSide,
    pub domain: DomainTag,
    pub contributing_wallets: Vec<String>,
    pub consensus_score: Decimal,
    pub target_vwap: Decimal,
    pub created_at: DateTime<Utc>,
}

/// Order execution side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Limit order execution proposal generated by the strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOrderProposal {
    pub proposal_id: String,
    pub market_id: String,
    pub token_id: String,
    /// Directional side of the token being traded (YES/NO).
    /// Used to keep risk accounting direction-aware and avoid mirroring
    /// exits on the wrong side of a market.
    pub outcome: OutcomeSide,
    pub side: OrderSide,
    pub limit_price: Decimal,
    pub shares: Decimal,
    pub notional_usdc: Decimal,
    pub ttl_seconds: u64,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

/// Lifecycle status of an executed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderLifecycleStatus {
    Pending,
    Active,
    PartiallyFilled,
    Filled,
    CancelledByTTL,
    Rejected,
    Failed,
}

/// Execution tracking state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionOrderState {
    pub order_id: String,
    pub proposal: ExecutionOrderProposal,
    pub status: OrderLifecycleStatus,
    pub filled_shares: Decimal,
    pub filled_avg_price: Decimal,
    pub placed_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_updated_at: DateTime<Utc>,
}

/// Complete strategy configuration parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletCopyTradingConfig {
    // 1. Whitelist Funnel Filters
    pub min_settled_events: usize,        // >= 50
    pub min_active_days: u32,             // >= 90
    pub min_avg_entry_price: Decimal,     // 0.20
    pub max_avg_entry_price: Decimal,     // 0.80
    pub max_single_profit_ratio: Decimal, // 0.25 (25%)
    pub max_maker_ratio: Decimal,         // 0.90 (90%)
    pub domain_volume_threshold: Decimal, // 0.60 (60% to get tagged)

    // 2. Market Filters
    pub max_spread: Decimal,          // 0.02 (2%)
    pub min_24h_volume_usdc: Decimal, // $50,000
    pub min_hours_to_expiry: Decimal, // 72.0 hours
    pub isolate_multi_outcome: bool,  // true (isolate >= 3 outcomes)

    // 3. Execution & Price Protection
    pub max_slippage_pct: Decimal,        // 0.015 (1.5%)
    pub max_price_offset_usd: Decimal,    // $0.015
    pub default_ttl_seconds: u64,         // 45 seconds (30..60s)
    pub max_top5_depth_pct: Decimal,      // 0.10 (10%)
    pub min_order_notional_usdc: Decimal, // $5.00

    // 4. Position Sizing & Risk Management
    pub single_trade_allocation_pct: Decimal, // 0.02 (2% of total portfolio)
    pub max_single_market_exposure_pct: Decimal, // 0.10 (10% max per market)
    pub max_total_exposure_pct: Decimal,      // 0.30 (30% max aggregate across all markets)
    pub daily_max_drawdown_pct: Decimal,      // 0.05 (5% daily killswitch)
    pub circuit_breaker_cooldown_hours: u32,  // 24 hours

    // 5. Consensus & Closed-Loop
    pub consensus_window_hours: i64,       // 12 hours
    pub min_consensus_wallets: usize,      // 2 wallets
    pub min_signal_notional_usdc: Decimal, // $100 minimum signal size for consensus counting
    /// PAPER-only research floor: smaller observations are shadowed but never
    /// enter consensus or reserve strategy capital.
    pub shadow_min_signal_notional_usdc: Decimal,
    pub min_signal_vwap: Decimal, // 0.05 sanity floor for consensus VWAP
    pub max_signal_vwap: Decimal, // 0.95 sanity ceiling for consensus VWAP
    pub max_signal_age_seconds: i64, // reject stale/replayed observations
    /// PAPER-only observation window. Signals older than the executable limit
    /// can still be shadowed without entering consensus or reserving capital.
    pub shadow_max_signal_age_seconds: i64,
    /// Delay before a PAPER proposal re-reads the live book and simulates a fill.
    pub paper_execution_delay_ms: u64,
    pub exit_trigger_reduction_pct: Decimal, // 0.30 (30% exit threshold)
    pub max_acceptable_brier_score: Decimal, // 0.25
    pub max_consecutive_direction_errors: u32, // 3 strikes
}

impl Default for WalletCopyTradingConfig {
    fn default() -> Self {
        Self {
            // Module 1
            min_settled_events: 50,
            min_active_days: 90,
            min_avg_entry_price: dec!(0.20),
            max_avg_entry_price: dec!(0.80),
            max_single_profit_ratio: dec!(0.25),
            max_maker_ratio: dec!(0.90),
            domain_volume_threshold: dec!(0.60),

            // Module 2
            max_spread: dec!(0.02),
            min_24h_volume_usdc: dec!(50000.0),
            min_hours_to_expiry: dec!(72.0),
            isolate_multi_outcome: true,

            // Module 3
            max_slippage_pct: dec!(0.015),
            max_price_offset_usd: dec!(0.015),
            default_ttl_seconds: 45,
            max_top5_depth_pct: dec!(0.10),
            min_order_notional_usdc: dec!(5.0),

            // Module 4
            single_trade_allocation_pct: dec!(0.02),
            max_single_market_exposure_pct: dec!(0.10),
            max_total_exposure_pct: dec!(0.30),
            daily_max_drawdown_pct: dec!(0.05),
            circuit_breaker_cooldown_hours: 24,

            // Module 5
            consensus_window_hours: 12,
            min_consensus_wallets: 2,
            min_signal_notional_usdc: dec!(100.0),
            shadow_min_signal_notional_usdc: dec!(10.0),
            min_signal_vwap: dec!(0.05),
            max_signal_vwap: dec!(0.95),
            max_signal_age_seconds: 120,
            shadow_max_signal_age_seconds: 300,
            paper_execution_delay_ms: 500,
            exit_trigger_reduction_pct: dec!(0.30),
            max_acceptable_brier_score: dec!(0.25),
            max_consecutive_direction_errors: 3,
        }
    }
}

impl WalletCopyTradingConfig {
    pub fn validate(&self) -> Result<(), String> {
        let fraction = |name: &str, value: Decimal| {
            (value > Decimal::ZERO && value <= Decimal::ONE)
                .then_some(())
                .ok_or_else(|| format!("{name} must be in (0, 1], got {value}"))
        };
        fraction(
            "single_trade_allocation_pct",
            self.single_trade_allocation_pct,
        )?;
        fraction(
            "max_single_market_exposure_pct",
            self.max_single_market_exposure_pct,
        )?;
        fraction("max_total_exposure_pct", self.max_total_exposure_pct)?;
        fraction("daily_max_drawdown_pct", self.daily_max_drawdown_pct)?;
        fraction("max_top5_depth_pct", self.max_top5_depth_pct)?;
        if self.single_trade_allocation_pct > self.max_single_market_exposure_pct {
            return Err("single trade allocation exceeds per-market cap".to_string());
        }
        if self.max_single_market_exposure_pct > self.max_total_exposure_pct {
            return Err("per-market cap exceeds portfolio cap".to_string());
        }
        if self.min_signal_vwap <= Decimal::ZERO
            || self.max_signal_vwap >= Decimal::ONE
            || self.min_signal_vwap >= self.max_signal_vwap
        {
            return Err("signal VWAP bounds must satisfy 0 < min < max < 1".to_string());
        }
        if self.min_consensus_wallets == 0 || self.default_ttl_seconds == 0 {
            return Err("consensus wallet count and TTL must be positive".to_string());
        }
        if self.shadow_min_signal_notional_usdc <= Decimal::ZERO
            || self.shadow_min_signal_notional_usdc > self.min_signal_notional_usdc
        {
            return Err(
                "shadow signal notional must be positive and cannot exceed live minimum"
                    .to_string(),
            );
        }
        if self.max_signal_age_seconds <= 0
            || self.shadow_max_signal_age_seconds < self.max_signal_age_seconds
        {
            return Err(
                "shadow signal age must be >= the positive executable signal age".to_string(),
            );
        }
        Ok(())
    }

    /// Loads configuration from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_SETTLED_EVENTS") {
            if let Ok(val) = v.trim().parse::<usize>() {
                cfg.min_settled_events = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_ACTIVE_DAYS") {
            if let Ok(val) = v.trim().parse::<u32>() {
                cfg.min_active_days = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_AVG_ENTRY_PRICE") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_avg_entry_price = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_AVG_ENTRY_PRICE") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_avg_entry_price = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SINGLE_PROFIT_RATIO") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_single_profit_ratio = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_MAKER_RATIO") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_maker_ratio = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SPREAD") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_spread = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_24H_VOLUME_USDC") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_24h_volume_usdc = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_HOURS_TO_EXPIRY") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_hours_to_expiry = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SLIPPAGE_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_slippage_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_PRICE_OFFSET_USD") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_price_offset_usd = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_DEFAULT_TTL_SECONDS") {
            if let Ok(val) = v.trim().parse::<u64>() {
                cfg.default_ttl_seconds = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_TOP5_DEPTH_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_top5_depth_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_SINGLE_TRADE_ALLOCATION_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.single_trade_allocation_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SINGLE_MARKET_EXPOSURE_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_single_market_exposure_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_TOTAL_EXPOSURE_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_total_exposure_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_DAILY_MAX_DRAWDOWN_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.daily_max_drawdown_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_CONSENSUS_WINDOW_HOURS") {
            if let Ok(val) = v.trim().parse::<i64>() {
                cfg.consensus_window_hours = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_CONSENSUS_WALLETS") {
            if let Ok(val) = v.trim().parse::<usize>() {
                cfg.min_consensus_wallets = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_SIGNAL_NOTIONAL_USDC") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_signal_notional_usdc = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_SHADOW_MIN_SIGNAL_NOTIONAL_USDC") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.shadow_min_signal_notional_usdc = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_SIGNAL_VWAP") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_signal_vwap = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SIGNAL_VWAP") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_signal_vwap = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_SIGNAL_AGE_SECONDS") {
            if let Ok(val) = v.trim().parse::<i64>() {
                cfg.max_signal_age_seconds = val.max(1);
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_SHADOW_MAX_SIGNAL_AGE_SECONDS") {
            if let Ok(val) = v.trim().parse::<i64>() {
                cfg.shadow_max_signal_age_seconds = val.max(1);
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_PAPER_EXECUTION_DELAY_MS") {
            if let Ok(val) = v.trim().parse::<u64>() {
                cfg.paper_execution_delay_ms = val.min(30_000);
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_EXIT_TRIGGER_REDUCTION_PCT") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.exit_trigger_reduction_pct = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_ISOLATE_MULTI_OUTCOME") {
            if let Ok(val) = v.trim().parse::<bool>() {
                cfg.isolate_multi_outcome = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MIN_ORDER_NOTIONAL_USDC") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.min_order_notional_usdc = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_CIRCUIT_BREAKER_COOLDOWN_HOURS") {
            if let Ok(val) = v.trim().parse::<u32>() {
                cfg.circuit_breaker_cooldown_hours = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_ACCEPTABLE_BRIER_SCORE") {
            if let Ok(val) = v.trim().parse::<Decimal>() {
                cfg.max_acceptable_brier_score = val;
            }
        }
        if let Ok(v) = std::env::var("WALLET_COPY_TRADING_MAX_CONSECUTIVE_DIRECTION_ERRORS") {
            if let Ok(val) = v.trim().parse::<u32>() {
                cfg.max_consecutive_direction_errors = val;
            }
        }

        cfg
    }
}
