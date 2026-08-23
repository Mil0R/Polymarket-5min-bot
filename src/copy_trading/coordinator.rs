use crate::copy_trading::consensus::ConsensusEngine;
use crate::copy_trading::execution::SmartExecutionEngine;
use crate::copy_trading::exit_engine::ExitEngine;
use crate::copy_trading::market_filter::MarketFilterEngine;
use crate::copy_trading::risk::RiskController;
use crate::copy_trading::types::{
    CopyTradingConfig, ExecutionOrderProposal, MarketMetadata, OrderBookSnapshot, OrderSide,
    OutcomeSide, SignalAction, SignalEvent,
};
use crate::copy_trading::whitelist::WhitelistManager;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// High-level coordinator unifying all 5 modules of the Polymarket copy-trading strategy.
#[derive(Debug, Clone)]
pub struct CopyTradingCoordinator {
    config: CopyTradingConfig,
    whitelist: WhitelistManager,
    market_filter: MarketFilterEngine,
    execution: SmartExecutionEngine,
    risk: Arc<RwLock<RiskController>>,
    consensus: ConsensusEngine,
    exit_engine: ExitEngine,
}

impl CopyTradingCoordinator {
    pub fn new(config: CopyTradingConfig, initial_nav: Decimal, now: DateTime<Utc>) -> Self {
        let whitelist = WhitelistManager::new(config.clone());
        let market_filter = MarketFilterEngine::new(config.clone());
        let execution = SmartExecutionEngine::new(config.clone());
        let risk = Arc::new(RwLock::new(RiskController::new(config.clone(), initial_nav, now)));
        let consensus = ConsensusEngine::new(config.clone(), whitelist.clone());
        let exit_engine = ExitEngine::new(config.clone(), whitelist.clone());

        Self {
            config,
            whitelist,
            market_filter,
            execution,
            risk,
            consensus,
            exit_engine,
        }
    }

    pub fn config(&self) -> &CopyTradingConfig {
        &self.config
    }

    pub fn whitelist(&self) -> &WhitelistManager {
        &self.whitelist
    }

    pub fn market_filter(&self) -> &MarketFilterEngine {
        &self.market_filter
    }

    pub fn execution(&self) -> &SmartExecutionEngine {
        &self.execution
    }

    pub fn risk(&self) -> &Arc<RwLock<RiskController>> {
        &self.risk
    }

    pub fn consensus(&self) -> &ConsensusEngine {
        &self.consensus
    }

    pub fn exit_engine(&self) -> &ExitEngine {
        &self.exit_engine
    }

    /// Primary event pipeline: processes an incoming target wallet activity signal.
    /// Handles filtering, consensus, market validation, risk allocation, and safe limit order proposal generation.
    pub async fn process_signal(
        &self,
        signal: SignalEvent,
        market: &MarketMetadata,
        book: &OrderBookSnapshot,
        now: DateTime<Utc>,
    ) -> Result<Option<ExecutionOrderProposal>, String> {
        // 1. Exit / Reduction action routing
        if signal.action != SignalAction::OpenBuy {
            let risk_guard = self.risk.read().await;
            if let Some(exit_prop) = self.exit_engine.handle_exit_signal(&signal, book, &risk_guard, now) {
                return Ok(Some(exit_prop));
            }
            return Ok(None);
        }

        // 2. Market Admission Filtering (Module 2)
        self.market_filter
            .validate_market(market, book, now)
            .map_err(|e| format!("Market validation failed: {}", e))?;

        // 3. Multi-source Consensus & Signal Aggregation (Module 5 & 1)
        let consensus_opt = self.consensus.ingest_signal(signal, now);
        let consensus = match consensus_opt {
            Some(cs) => cs,
            None => return Ok(None), // Waiting for partner signal or not authorized
        };

        // 4. Portfolio Allocation & Drawdown Risk Check (Module 4)
        let mut risk_guard = self.risk.write().await;
        let budget_usdc = risk_guard
            .allocate_trade_budget(&consensus.market_id, now)
            .map_err(|e| format!("Risk allocation rejected: {}", e))?;

        // 5. Smart Limit Order Pricing & Top-5 Depth Protection (Module 3)
        let proposal = self
            .execution
            .build_buy_proposal(&consensus, book, budget_usdc, now)
            .map_err(|e| format!("Execution pricing rejected: {}", e))?;

        info!(
            proposal_id = %proposal.proposal_id,
            market_id = %proposal.market_id,
            limit_price = %proposal.limit_price,
            shares = %proposal.shares,
            notional = %proposal.notional_usdc,
            "Successfully generated validated copy-trade execution proposal"
        );

        Ok(Some(proposal))
    }

    /// Records order placement and starts TTL tracking.
    pub fn on_order_placed(&self, order_id: String, proposal: ExecutionOrderProposal, placed_at: DateTime<Utc>) {
        self.execution.register_placed_order(order_id, proposal, placed_at);
    }

    /// Handles order fill event from CLOB WebSocket or API.
    ///
    /// The fill is routed by the registered order's side:
    ///   - Buy:  records a new BUY position (keyed by token, direction-aware)
    ///   - Sell: records a SELL / position reduction (keyed by token)
    ///
    /// `filled_shares` must be the size of THIS fill event (incremental).
    pub async fn on_order_fill(
        &self,
        order_id: &str,
        market_id: &str,
        token_id: &str,
        filled_shares: Decimal,
        fill_price: Decimal,
        now: DateTime<Utc>,
    ) {
        self.execution.record_fill(order_id, filled_shares, fill_price, now);

        let order_side = self
            .execution
            .get_order_state(order_id)
            .map(|s| s.proposal.side);
        let outcome = self
            .execution
            .get_order_state(order_id)
            .map(|s| s.proposal.outcome);

        let risk_guard = self.risk.read().await;
        match order_side {
            Some(OrderSide::Buy) => {
                let outcome = outcome.unwrap_or(OutcomeSide::Other("unknown".to_string()));
                risk_guard.record_position_fill(market_id, token_id, &outcome, filled_shares, fill_price);
            }
            Some(OrderSide::Sell) => {
                risk_guard.record_position_reduction(token_id, filled_shares, fill_price);
            }
            None => {
                warn!(
                    order_id = %order_id,
                    "Fill received for unknown order; skipped position accounting"
                );
            }
        }
    }

    /// Periodic heartbeat check for order TTL expiration and background cleanup.
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        // 1. Check TTL expired orders requiring immediate cancellation
        let expired_orders = self.execution.check_ttl_expirations(now);
        if !expired_orders.is_empty() {
            warn!(count = expired_orders.len(), "Identified TTL expired orders to cancel");
        }

        // 2. Risk rollover & daily drawdown check
        let mut risk_guard = self.risk.write().await;
        risk_guard.check_daily_rollover(now);
        risk_guard.check_and_apply_drawdown_breaker(now);

        // 3. Prune consensus accumulator
        self.consensus.prune_stale_buckets(now);

        expired_orders
    }

    /// Handles market resolution event: applies rolling Brier Score updates & 3-strike
    /// rules for the whitelist, AND settles our own positions in that market to their
    /// final 1.0/0.0 value so NAV reflects the realized outcome.
    pub async fn on_market_settlement(
        &self,
        market_id: &str,
        resolved_outcome: &OutcomeSide,
        wallet_trades: &[(String, OutcomeSide, Decimal)],
    ) {
        self.exit_engine
            .handle_market_settlement(market_id, resolved_outcome, wallet_trades);

        let risk_guard = self.risk.write().await;
        risk_guard.settle_market_positions(market_id, resolved_outcome);
    }
}
