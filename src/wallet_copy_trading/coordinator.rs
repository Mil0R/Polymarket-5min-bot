use crate::wallet_copy_trading::consensus::ConsensusEngine;
use crate::wallet_copy_trading::execution::SmartExecutionEngine;
use crate::wallet_copy_trading::exit_engine::ExitEngine;
use crate::wallet_copy_trading::market_filter::MarketFilterEngine;
use crate::wallet_copy_trading::risk::RiskController;
use crate::wallet_copy_trading::types::{
    ExecutionOrderProposal, MarketMetadata, OrderBookSnapshot, OrderSide, OutcomeSide,
    SignalAction, SignalEvent, WalletCopyTradingConfig,
};
use crate::wallet_copy_trading::whitelist::WhitelistManager;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

type SettlementAttribution = (String, OutcomeSide, Decimal);
type SettlementAttributionMap = DashMap<String, Vec<SettlementAttribution>>;

/// High-level coordinator unifying all 5 modules of the Polymarket wallet-copy-trading strategy.
#[derive(Debug, Clone)]
pub struct WalletCopyTradingCoordinator {
    config: WalletCopyTradingConfig,
    whitelist: WhitelistManager,
    market_filter: MarketFilterEngine,
    execution: SmartExecutionEngine,
    risk: Arc<RwLock<RiskController>>,
    consensus: ConsensusEngine,
    exit_engine: ExitEngine,
    settlement_attributions: Arc<SettlementAttributionMap>,
    attribution_state_path: Option<PathBuf>,
}

impl WalletCopyTradingCoordinator {
    pub fn new(config: WalletCopyTradingConfig, initial_nav: Decimal, now: DateTime<Utc>) -> Self {
        let whitelist = WhitelistManager::new(config.clone());
        let market_filter = MarketFilterEngine::new(config.clone());
        let execution = SmartExecutionEngine::new(config.clone());
        let risk = Arc::new(RwLock::new(RiskController::new(
            config.clone(),
            initial_nav,
            now,
        )));
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
            settlement_attributions: Arc::new(DashMap::new()),
            attribution_state_path: None,
        }
    }

    /// Variant with whitelist persistence (Opinion 4): wallet profiles and
    /// 3-strike counters survive restarts via a JSON state file.
    pub fn new_with_whitelist_state(
        config: WalletCopyTradingConfig,
        initial_nav: Decimal,
        now: DateTime<Utc>,
        state_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        let whitelist = WhitelistManager::new_with_state(config.clone(), state_path);
        let market_filter = MarketFilterEngine::new(config.clone());
        let execution = SmartExecutionEngine::new(config.clone());
        let risk = Arc::new(RwLock::new(RiskController::new(
            config.clone(),
            initial_nav,
            now,
        )));
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
            settlement_attributions: Arc::new(DashMap::new()),
            attribution_state_path: None,
        }
    }

    pub fn new_with_state_paths(
        config: WalletCopyTradingConfig,
        initial_nav: Decimal,
        now: DateTime<Utc>,
        whitelist_state_path: impl Into<std::path::PathBuf>,
        risk_state_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        let risk_state_path = risk_state_path.into();
        let attribution_state_path = risk_state_path.with_extension("attributions.json");
        let whitelist = WhitelistManager::new_with_state(config.clone(), whitelist_state_path);
        let market_filter = MarketFilterEngine::new(config.clone());
        let execution = SmartExecutionEngine::new(config.clone());
        let risk = Arc::new(RwLock::new(RiskController::new_with_state(
            config.clone(),
            initial_nav,
            now,
            risk_state_path,
        )));
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
            settlement_attributions: Arc::new(Self::load_attributions(&attribution_state_path)),
            attribution_state_path: Some(attribution_state_path),
        }
    }

    fn load_attributions(path: &Path) -> SettlementAttributionMap {
        let map = DashMap::new();
        if let Ok(raw) = std::fs::read(path) {
            if let Ok(saved) = serde_json::from_slice::<
                std::collections::HashMap<String, Vec<SettlementAttribution>>,
            >(&raw)
            {
                for (market, rows) in saved {
                    map.insert(market, rows);
                }
            }
        }
        map
    }

    fn save_attributions(&self) {
        let Some(path) = &self.attribution_state_path else {
            return;
        };
        let snapshot: std::collections::HashMap<_, _> = self
            .settlement_attributions
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(
                &tmp,
                serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other)?,
            )?;
            std::fs::rename(tmp, path)
        };
        if let Err(e) = write() {
            warn!(path = %path.display(), error = %e, "Settlement attribution persistence failed");
        }
    }

    pub fn config(&self) -> &WalletCopyTradingConfig {
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
        market: Option<&MarketMetadata>,
        book: &OrderBookSnapshot,
        now: DateTime<Utc>,
    ) -> Result<Option<ExecutionOrderProposal>, String> {
        // 1. Exit / Reduction action routing (does not need market metadata)
        if signal.action != SignalAction::OpenBuy {
            let risk_guard = self.risk.read().await;
            if let Some(exit_prop) =
                self.exit_engine
                    .handle_exit_signal(&signal, book, &risk_guard, now)
            {
                return Ok(Some(exit_prop));
            }
            return Ok(None);
        }

        // 2. Market Admission Filtering (Module 2) — requires metadata
        let market =
            market.ok_or_else(|| "Market metadata unavailable for buy signal".to_string())?;
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
        if !risk_guard.reserve_order(
            &proposal.proposal_id,
            &proposal.market_id,
            proposal.notional_usdc,
        ) {
            return Err("Risk reservation failed for proposal".to_string());
        }
        {
            let mut rows = self
                .settlement_attributions
                .entry(consensus.market_id.clone())
                .or_default();
            for wallet in &consensus.contributing_wallets {
                if !rows
                    .iter()
                    .any(|(existing, _, _)| existing.eq_ignore_ascii_case(wallet))
                {
                    rows.push((
                        wallet.clone(),
                        consensus.outcome.clone(),
                        consensus.target_vwap,
                    ));
                }
            }
        }
        self.save_attributions();
        self.consensus.commit_consensus(&consensus, now);

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
    pub fn on_order_placed(
        &self,
        order_id: String,
        proposal: ExecutionOrderProposal,
        placed_at: DateTime<Utc>,
    ) {
        self.execution
            .register_placed_order(order_id, proposal, placed_at);
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
        let accepted_fill = self
            .execution
            .record_fill(order_id, filled_shares, fill_price, now);
        if accepted_fill <= Decimal::ZERO {
            warn!(order_id = %order_id, "Fill ignored: unknown, duplicate, or exceeds proposal size");
            return;
        }

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
                risk_guard.record_position_fill(
                    market_id,
                    token_id,
                    &outcome,
                    accepted_fill,
                    fill_price,
                );
            }
            Some(OrderSide::Sell) => {
                risk_guard.record_position_reduction(token_id, accepted_fill, fill_price);
            }
            None => {
                warn!(
                    order_id = %order_id,
                    "Fill received for unknown order; skipped position accounting"
                );
            }
        }
    }

    /// Marks an immediate-or-cancel order terminal and releases its unused risk
    /// reservation. Safe to call for rejected submission attempts as well.
    pub async fn on_order_finalized(
        &self,
        order_id: Option<&str>,
        proposal_id: &str,
        now: DateTime<Utc>,
    ) {
        if let Some(order_id) = order_id {
            self.execution.finalize_order(order_id, now);
        }
        self.risk
            .read()
            .await
            .release_order_reservation(proposal_id);
    }

    /// Periodic heartbeat check for order TTL expiration and background cleanup.
    pub async fn tick(&self, now: DateTime<Utc>) -> Vec<String> {
        // 1. Check TTL expired orders requiring immediate cancellation
        let expired_orders = self.execution.check_ttl_expirations(now);
        if !expired_orders.is_empty() {
            warn!(
                count = expired_orders.len(),
                "Identified TTL expired orders to cancel"
            );
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
        let stored;
        let wallet_trades = if wallet_trades.is_empty() {
            stored = self
                .settlement_attributions
                .remove(market_id)
                .map(|(_, rows)| rows)
                .unwrap_or_default();
            self.save_attributions();
            stored.as_slice()
        } else {
            wallet_trades
        };
        self.exit_engine
            .handle_market_settlement(market_id, resolved_outcome, wallet_trades);

        let risk_guard = self.risk.write().await;
        risk_guard.settle_market_positions(market_id, resolved_outcome);
    }
}
