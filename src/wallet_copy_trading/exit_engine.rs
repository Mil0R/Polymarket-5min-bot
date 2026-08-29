use crate::wallet_copy_trading::risk::RiskController;
use crate::wallet_copy_trading::types::{
    ExecutionOrderProposal, OrderBookSnapshot, OrderSide, OutcomeSide, SignalAction, SignalEvent,
    WalletCopyTradingConfig,
};
use crate::wallet_copy_trading::whitelist::WhitelistManager;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Exit engine handling target wallet exit mirroring and rolling evaluation.
#[derive(Debug, Clone)]
pub struct ExitEngine {
    config: WalletCopyTradingConfig,
    whitelist: WhitelistManager,
}

impl ExitEngine {
    pub fn new(config: WalletCopyTradingConfig, whitelist: WhitelistManager) -> Self {
        Self { config, whitelist }
    }

    /// Evaluates incoming wallet activity for exit or reduction actions.
    /// If an exit condition is met, generates a safe exit limit order proposal.
    ///
    /// Hard rules (fixes for direction/token mismatch):
    ///   1. The signal source wallet must be whitelisted & authorized in the domain
    ///      (a blacklisted/unknown wallet must never be able to force us to exit).
    ///   2. The exit is only mirrored if it matches a position we actually hold in
    ///      the SAME (market, outcome) direction.
    ///   3. The sell order's token_id always comes from OUR exposure, never from the
    ///      signal (which could reference the opposite side of the market).
    ///   4. Exit size is capped at Top-5 bid depth so we never dump into the book.
    pub fn handle_exit_signal(
        &self,
        signal: &SignalEvent,
        book: &OrderBookSnapshot,
        risk: &RiskController,
        now: DateTime<Utc>,
    ) -> Option<ExecutionOrderProposal> {
        // 1. Check if signal is an exit or position reduction
        let is_exit_action = matches!(
            signal.action,
            SignalAction::ScaleOut | SignalAction::CloseSell | SignalAction::ReversePosition
        );

        if !is_exit_action {
            return None;
        }

        // A SELL without a reliable pre-trade position denominator is ambiguous:
        // it could be a 1% trim or a full liquidation. Fail closed instead of
        // guessing. ReversePosition remains an explicit full-exit instruction.
        if matches!(
            signal.action,
            SignalAction::CloseSell | SignalAction::ScaleOut
        ) && signal.target_reduction_pct.is_none()
        {
            debug!(
                wallet = %signal.source_wallet,
                "Exit signal ignored: source position reduction ratio unavailable"
            );
            return None;
        }

        // 2. Signal source must be an authorized whitelisted wallet in this domain.
        let (is_auth, _) = self
            .whitelist
            .is_authorized_in_domain(&signal.source_wallet, &signal.domain);
        if !is_auth {
            debug!(
                wallet = %signal.source_wallet,
                domain = %signal.domain,
                "Exit signal ignored: source wallet not authorized in domain"
            );
            return None;
        }

        // 3. Direction-aware exposure lookup: only mirror exits for positions we
        //    actually hold on the SAME (market, outcome) side.
        let exposure = risk.get_market_exposure(&signal.market_id, &signal.outcome)?;
        if exposure.shares <= Decimal::ZERO {
            return None;
        }

        // 4. Best Bid pricing for passive exit execution.
        let best_bid = book.best_bid().unwrap_or(exposure.avg_entry_price);
        if best_bid <= Decimal::ZERO {
            warn!(market_id = %signal.market_id, "No bid liquidity available for exit order");
            return None;
        }

        // 5. Calculate desired exit shares based on reduction fraction.
        let desired_shares = match signal.action {
            SignalAction::ReversePosition => exposure.shares,
            SignalAction::CloseSell | SignalAction::ScaleOut => {
                let ratio = signal
                    .target_reduction_pct
                    .unwrap_or(self.config.exit_trigger_reduction_pct)
                    .clamp(Decimal::ZERO, Decimal::ONE);
                (exposure.shares * ratio).min(exposure.shares)
            }
            _ => Decimal::ZERO,
        };

        if desired_shares <= Decimal::ZERO {
            return None;
        }

        // 6. Anti-dump cap: never sell more than Top-5 bid depth * max_top5_depth_pct.
        let top5_bid_notional = book.top_n_bid_depth_notional(5);
        let mut exit_shares = desired_shares;
        if top5_bid_notional > Decimal::ZERO {
            let max_exit_notional = top5_bid_notional * self.config.max_top5_depth_pct;
            let max_exit_shares = max_exit_notional / best_bid;
            if desired_shares > max_exit_shares {
                exit_shares = max_exit_shares;
                warn!(
                    market_id = %signal.market_id,
                    token_id = %exposure.token_id,
                    desired_shares = %desired_shares,
                    capped_shares = %exit_shares,
                    top5_bid_notional = %top5_bid_notional,
                    "Exit size capped by Top-5 bid depth (anti-dump)"
                );
            }
        } else {
            warn!(market_id = %signal.market_id, "No bid depth for exit sizing; refusing to exit");
            return None;
        }

        if exit_shares <= Decimal::ZERO {
            return None;
        }

        let notional = (exit_shares * best_bid).round_dp(2);
        let proposal_id = format!("exit-prop-{}", Uuid::new_v4());

        info!(
            market_id = %signal.market_id,
            token_id = %exposure.token_id,
            action = ?signal.action,
            exit_shares = %exit_shares,
            best_bid = %best_bid,
            notional = %notional,
            "Mirroring target wallet exit: generated exit limit order"
        );

        Some(ExecutionOrderProposal {
            proposal_id,
            market_id: signal.market_id.clone(),
            token_id: exposure.token_id.clone(), // OUR token, never the signal's
            outcome: exposure.outcome.clone(),
            side: OrderSide::Sell,
            limit_price: best_bid,
            shares: exit_shares,
            notional_usdc: notional,
            ttl_seconds: self.config.default_ttl_seconds,
            reason: format!(
                "Exit Mirroring: Target wallet {} executed {:?} on market {} ({} side)",
                signal.source_wallet, signal.action, signal.market_id, exposure.outcome
            ),
            created_at: now,
        })
    }

    /// Handles market resolution event, updating Brier scores and checking 3-strike rules.
    pub fn handle_market_settlement(
        &self,
        market_id: &str,
        resolved_outcome: &OutcomeSide,
        participating_wallets: &[(String, OutcomeSide, Decimal)], // (address, side_bought, entry_price)
    ) {
        for (address, side_bought, entry_price) in participating_wallets {
            let won = side_bought == resolved_outcome;
            // Major direction misjudgment rule: bought at high implied probability (> 0.70) but lost
            let is_major_error = !won && *entry_price >= dec!(0.70);

            self.whitelist
                .record_direction_result(address, is_major_error);

            info!(
                address = %address,
                market_id = %market_id,
                won = won,
                is_major_error = is_major_error,
                "Updated settlement tracking for wallet"
            );
        }
    }
}
