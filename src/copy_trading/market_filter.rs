use crate::copy_trading::types::{CopyTradingConfig, MarketMetadata, OrderBookSnapshot};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::fmt;
use tracing::{debug, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketFilterError {
    ComplexMarketIsolated {
        outcome_count: usize,
        is_neg_risk: bool,
    },
    ExpiryTooSoon {
        hours_left_i64: i64,
        min_hours: u32,
    },
    Insufficient24hVolume {
        volume_24h: Decimal,
        min_volume: Decimal,
    },
    SpreadTooWide {
        spread_pct: Decimal,
        max_spread_pct: Decimal,
    },
    EmptyOrderBook {
        bid_count: usize,
        ask_count: usize,
    },
    MarketInactive,
}

impl fmt::Display for MarketFilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketFilterError::ComplexMarketIsolated {
                outcome_count,
                is_neg_risk,
            } => write!(
                f,
                "Multi-outcome or NegRisk market rejected (outcomes: {}, is_neg_risk: {})",
                outcome_count, is_neg_risk
            ),
            MarketFilterError::ExpiryTooSoon {
                hours_left_i64,
                min_hours,
            } => write!(
                f,
                "Time to expiry {}h is below minimum {}h threshold",
                hours_left_i64, min_hours
            ),
            MarketFilterError::Insufficient24hVolume {
                volume_24h,
                min_volume,
            } => write!(
                f,
                "Market 24h volume ${} is below minimum ${}",
                volume_24h, min_volume
            ),
            MarketFilterError::SpreadTooWide {
                spread_pct,
                max_spread_pct,
            } => write!(
                f,
                "Orderbook spread {}% exceeds maximum allowed {}%",
                spread_pct, max_spread_pct
            ),
            MarketFilterError::EmptyOrderBook {
                bid_count,
                ask_count,
            } => write!(
                f,
                "Missing or empty orderbook liquidity (bids: {}, asks: {})",
                bid_count, ask_count
            ),
            MarketFilterError::MarketInactive => write!(f, "Market is marked inactive or resolved"),
        }
    }
}

impl std::error::Error for MarketFilterError {}

/// Engine responsible for validating whether a target market qualifies for copy-trading.
#[derive(Debug, Clone)]
pub struct MarketFilterEngine {
    config: CopyTradingConfig,
}

impl MarketFilterEngine {
    pub fn new(config: CopyTradingConfig) -> Self {
        Self { config }
    }

    /// Evaluates market metadata and real-time orderbook against all Module 2 admission criteria.
    pub fn validate_market(
        &self,
        market: &MarketMetadata,
        book: &OrderBookSnapshot,
        now: DateTime<Utc>,
    ) -> Result<(), MarketFilterError> {
        // 1. Market active status
        if !market.is_active {
            return Err(MarketFilterError::MarketInactive);
        }

        // 2. Complex Multi-Outcome / NegRisk Isolation
        if self.config.isolate_multi_outcome && (market.outcome_count >= 3 || market.is_neg_risk) {
            warn!(
                market_id = %market.market_id,
                outcomes = market.outcome_count,
                neg_risk = market.is_neg_risk,
                "Market rejected: complex multi-outcome isolation active"
            );
            return Err(MarketFilterError::ComplexMarketIsolated {
                outcome_count: market.outcome_count,
                is_neg_risk: market.is_neg_risk,
            });
        }

        // 3. Expiry Window Limitation (>= config.min_hours_to_expiry)
        let duration_to_expiry = market.settlement_time - now;
        let hours_left_i64 = duration_to_expiry.num_hours();
        let min_hours = self.config.min_hours_to_expiry;
        let min_hours_u32 = min_hours.round().to_u32().unwrap_or(72);

        if Decimal::from(hours_left_i64) < min_hours {
            debug!(
                market_id = %market.market_id,
                hours_left = %hours_left_i64,
                min_hours = %min_hours_u32,
                "Market rejected: expiry window below minimum (short-term/settling market)"
            );
            return Err(MarketFilterError::ExpiryTooSoon {
                hours_left_i64,
                min_hours: min_hours_u32,
            });
        }

        // 4. 24h Trading Volume Check
        if market.volume_24h_usdc < self.config.min_24h_volume_usdc {
            debug!(
                market_id = %market.market_id,
                volume_24h = %market.volume_24h_usdc,
                min_vol = %self.config.min_24h_volume_usdc,
                "Market rejected: 24h volume too low"
            );
            return Err(MarketFilterError::Insufficient24hVolume {
                volume_24h: market.volume_24h_usdc,
                min_volume: self.config.min_24h_volume_usdc,
            });
        }

        // 5. Orderbook Liquidity & Spread Checks
        if book.bids.is_empty() || book.asks.is_empty() {
            return Err(MarketFilterError::EmptyOrderBook {
                bid_count: book.bids.len(),
                ask_count: book.asks.len(),
            });
        }

        let spread = book.relative_spread().ok_or(MarketFilterError::EmptyOrderBook {
            bid_count: book.bids.len(),
            ask_count: book.asks.len(),
        })?;

        if spread > self.config.max_spread {
            debug!(
                market_id = %market.market_id,
                spread = %(spread * dec!(100)),
                max_spread = %(self.config.max_spread * dec!(100)),
                "Market rejected: orderbook spread exceeds limit"
            );
            return Err(MarketFilterError::SpreadTooWide {
                spread_pct: spread * dec!(100),
                max_spread_pct: self.config.max_spread * dec!(100),
            });
        }

        Ok(())
    }
}
