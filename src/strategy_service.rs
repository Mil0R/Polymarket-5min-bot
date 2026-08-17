use crate::live_safety::LiveSafetyGuard;
use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::observability_server::ServiceHealth;
use crate::risk::positions::ExposureReservation;
use crate::risk::recovery::RecoveryAction;
use crate::risk::{HedgeMonitor, RiskManager};
use crate::runtime_metrics::RuntimeMetrics;
use crate::trading::executor::{ExecutionQuote, UncertainWriteError};
use crate::trading::TradingGateway;
use polypulse::ui::{decimal_to_f64, DashboardHandle};

use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Owns the post-admission execution workflow: submit both legs, update the UI,
/// register fills, and hand imbalances to recovery monitoring.
#[derive(Clone)]
pub struct StrategyService {
    executor: Arc<dyn TradingGateway>,
    risk_manager: Arc<RiskManager>,
    hedge_monitor: Arc<HedgeMonitor>,
    dashboard: DashboardHandle,
    metrics: Arc<RuntimeMetrics>,
    live_safety: Arc<LiveSafetyGuard>,
    service_health: Arc<ServiceHealth>,
    dry_run: bool,
}

pub struct StrategyServiceDeps {
    pub executor: Arc<dyn TradingGateway>,
    pub risk_manager: Arc<RiskManager>,
    pub hedge_monitor: Arc<HedgeMonitor>,
    pub dashboard: DashboardHandle,
    pub metrics: Arc<RuntimeMetrics>,
    pub live_safety: Arc<LiveSafetyGuard>,
    pub service_health: Arc<ServiceHealth>,
    pub dry_run: bool,
}

impl StrategyService {
    pub fn new(deps: StrategyServiceDeps) -> Self {
        Self {
            executor: deps.executor,
            risk_manager: deps.risk_manager,
            hedge_monitor: deps.hedge_monitor,
            dashboard: deps.dashboard,
            metrics: deps.metrics,
            live_safety: deps.live_safety,
            service_health: deps.service_health,
            dry_run: deps.dry_run,
        }
    }

    pub async fn execute(
        self,
        opportunity: ArbitrageOpportunity,
        quote: ExecutionQuote,
        symbol: String,
        reservation: ExposureReservation,
    ) {
        let _reservation = reservation;
        self.metrics.record_trade_started();
        match self.executor.execute_pair(&opportunity, &quote).await {
            Ok(result) => {
                self.live_safety.record_result(&result);
                self.service_health
                    .set_safety_ok(!self.live_safety.is_tripped());
                let matched_fill = result.yes_filled.min(result.no_filled);
                let realized_edge = dec!(1) - result.yes_price - result.no_price;
                let realized_profit = decimal_to_f64(realized_edge * matched_fill);
                self.dashboard.with_mut(|dashboard| {
                    if result.success {
                        self.metrics.record_trade_succeeded();
                        dashboard.record_trade_success(
                            &symbol,
                            realized_profit,
                            decimal_to_f64(realized_edge * dec!(100)),
                        );
                    } else {
                        self.metrics.record_trade_failed();
                        dashboard.record_trade_failure(&symbol, "orders not filled");
                    }
                });

                // Paper fills belong exclusively to the virtual ledger. Never
                // persist them as real owned orders or start a recovery sell.
                if self.dry_run {
                    self.metrics.record_paper_result(
                        result.yes_filled,
                        result.no_filled,
                        result.yes_price,
                        result.no_price,
                    );
                    return;
                }

                let pair_id = result.pair_id.clone();
                self.risk_manager.register_order_pair(
                    result,
                    opportunity.market_id,
                    opportunity.yes_token_id,
                    opportunity.no_token_id,
                );
                match self.risk_manager.handle_order_pair(&pair_id).await {
                    Ok(action) => {
                        if matches!(&action, RecoveryAction::MonitorForExit { .. }) {
                            if let Err(error) = self.hedge_monitor.add_position(&action) {
                                error!(%error, "Failed to start recovery monitor");
                            }
                        }
                        match action {
                            RecoveryAction::None => {}
                            RecoveryAction::MonitorForExit { .. } => {
                                info!(%pair_id, "Recovery monitor started")
                            }
                            RecoveryAction::ManualIntervention { reason } => {
                                warn!(%reason, "Manual intervention needed")
                            }
                        }
                    }
                    Err(error) => error!(%error, "Risk handling failed"),
                }
                self.service_health
                    .set_no_unresolved_orders(!self.risk_manager.has_unresolved_state());
            }
            Err(error) => {
                if error.downcast_ref::<UncertainWriteError>().is_some() {
                    self.live_safety.trip(error.to_string());
                }
                self.service_health
                    .set_safety_ok(!self.live_safety.is_tripped());
                self.metrics.record_trade_failed();
                let message = error.to_string();
                self.dashboard
                    .with_mut(|dashboard| dashboard.record_trade_failure(&symbol, &message));
                error!(%error, "Arbitrage execution failed");
            }
        }
    }
}
