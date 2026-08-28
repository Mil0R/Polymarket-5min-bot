use crate::config::Config;
use crate::monitor::arbitrage::ArbitrageOpportunity;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdmissionRejection {
    ExecutionSpread,
    TradingPaused,
    ConnectionUnhealthy,
    UnresolvedState,
    YesPriceTooLow,
    NoPriceTooLow,
    NearMarketEnd,
}

impl AdmissionRejection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionSpread => "execution_spread",
            Self::TradingPaused => "trading_paused",
            Self::ConnectionUnhealthy => "connection_unhealthy",
            Self::UnresolvedState => "unresolved_state",
            Self::YesPriceTooLow => "yes_price_too_low",
            Self::NoPriceTooLow => "no_price_too_low",
            Self::NearMarketEnd => "near_market_end",
        }
    }
}

pub struct AdmissionContext {
    pub total_price: Decimal,
    pub trading_paused: bool,
    pub connection_healthy: bool,
    pub unresolved_state: bool,
    pub market_end: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

pub struct OpportunityAdmissionService {
    execution_threshold: Decimal,
    min_yes_price: Decimal,
    min_no_price: Decimal,
    stop_before_end_seconds: i64,
    dry_run: bool,
}

impl OpportunityAdmissionService {
    pub(crate) fn new(
        execution_threshold: Decimal,
        min_yes_price: Decimal,
        min_no_price: Decimal,
        stop_before_end_seconds: i64,
        dry_run: bool,
    ) -> Self {
        Self {
            execution_threshold,
            min_yes_price,
            min_no_price,
            stop_before_end_seconds,
            dry_run,
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::new(
            dec!(1) - Decimal::try_from(config.arbitrage_execution_spread).unwrap_or(dec!(0.01)),
            Decimal::try_from(config.min_yes_price_threshold).unwrap_or(dec!(0)),
            Decimal::try_from(config.min_no_price_threshold).unwrap_or(dec!(0)),
            config.stop_arbitrage_before_end_minutes as i64 * 60,
            config.dry_run,
        )
    }

    pub fn evaluate(
        &self,
        opportunity: &ArbitrageOpportunity,
        context: AdmissionContext,
    ) -> Result<(), AdmissionRejection> {
        if context.total_price > self.execution_threshold {
            return Err(AdmissionRejection::ExecutionSpread);
        }
        if context.trading_paused {
            return Err(AdmissionRejection::TradingPaused);
        }
        if !self.dry_run && !context.connection_healthy {
            return Err(AdmissionRejection::ConnectionUnhealthy);
        }
        if !self.dry_run && context.unresolved_state {
            return Err(AdmissionRejection::UnresolvedState);
        }
        if opportunity.yes_ask_price < self.min_yes_price {
            return Err(AdmissionRejection::YesPriceTooLow);
        }
        if opportunity.no_ask_price < self.min_no_price {
            return Err(AdmissionRejection::NoPriceTooLow);
        }
        if self.stop_before_end_seconds > 0
            && context.market_end.is_some_and(|end| {
                end.signed_duration_since(context.now).num_seconds() <= self.stop_before_end_seconds
            })
        {
            return Err(AdmissionRejection::NearMarketEnd);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::types::{B256, U256};

    fn opportunity() -> ArbitrageOpportunity {
        ArbitrageOpportunity {
            market_id: B256::ZERO,
            yes_token_id: U256::from(1),
            no_token_id: U256::from(2),
            yes_ask_price: dec!(0.45),
            no_ask_price: dec!(0.50),
            total_cost: dec!(0.95),
            profit_percentage: dec!(5),
            yes_size: dec!(10),
            no_size: dec!(10),
            yes_book_timestamp_ms: 0,
            no_book_timestamp_ms: 0,
        }
    }

    fn service() -> OpportunityAdmissionService {
        OpportunityAdmissionService::new(dec!(0.99), dec!(0.10), dec!(0.10), 60, false)
    }

    fn context(now: DateTime<Utc>) -> AdmissionContext {
        AdmissionContext {
            total_price: dec!(0.95),
            trading_paused: false,
            connection_healthy: true,
            unresolved_state: false,
            market_end: Some(now + chrono::Duration::seconds(120)),
            now,
        }
    }

    #[test]
    fn accepts_safe_opportunity() {
        let now = Utc::now();
        assert_eq!(service().evaluate(&opportunity(), context(now)), Ok(()));
    }

    #[test]
    fn reports_specific_safety_rejection() {
        let now = Utc::now();
        let mut unsafe_context = context(now);
        unsafe_context.unresolved_state = true;
        assert_eq!(
            service().evaluate(&opportunity(), unsafe_context),
            Err(AdmissionRejection::UnresolvedState)
        );
    }

    #[test]
    fn rejects_near_market_end_and_low_prices() {
        let now = Utc::now();
        let mut near_end = context(now);
        near_end.market_end = Some(now + chrono::Duration::seconds(30));
        assert_eq!(
            service().evaluate(&opportunity(), near_end),
            Err(AdmissionRejection::NearMarketEnd)
        );

        let mut low = opportunity();
        low.yes_ask_price = dec!(0.05);
        assert_eq!(
            service().evaluate(&low, context(now)),
            Err(AdmissionRejection::YesPriceTooLow)
        );
    }
}
