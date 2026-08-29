//! Read-only paper execution against live CLOB order books.

use anyhow::Result;
use polymarket_client_sdk_v2::clob::types::response::OrderSummary;
use polymarket_client_sdk_v2::clob::types::OrderType;
use polymarket_client_sdk_v2::types::Decimal;
use rust_decimal_macros::dec;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct PaperSettings {
    pub initial_balance: Decimal,
    pub liquidity_haircut: Decimal,
    pub second_leg_delay_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaperFill {
    pub requested: Decimal,
    pub filled: Decimal,
    pub cost: Decimal,
    pub fee: Decimal,
    pub average_price: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaperSnapshot {
    pub initial_balance: Decimal,
    pub cash_remaining: Decimal,
    pub cash_reserved: Decimal,
    pub total_spent: Decimal,
    pub paired_shares: Decimal,
    pub unmatched_cost: Decimal,
    pub theoretical_pnl: Decimal,
    pub winner_realized_pnl: Decimal,
    pub winner_positions_open: u64,
    pub pairs_attempted: u64,
}

struct PaperLedger {
    cash_remaining: Decimal,
    cash_reserved: Decimal,
    total_spent: Decimal,
    paired_shares: Decimal,
    unmatched_cost: Decimal,
    theoretical_pnl: Decimal,
    winner_realized_pnl: Decimal,
    winner_positions_open: u64,
    pairs_attempted: u64,
}

pub struct PaperTradingEngine {
    settings: PaperSettings,
    ledger: Mutex<PaperLedger>,
}

impl PaperTradingEngine {
    pub fn new(settings: PaperSettings) -> Self {
        Self {
            ledger: Mutex::new(PaperLedger {
                cash_remaining: settings.initial_balance,
                cash_reserved: dec!(0),
                total_spent: dec!(0),
                paired_shares: dec!(0),
                unmatched_cost: dec!(0),
                theoretical_pnl: dec!(0),
                winner_realized_pnl: dec!(0),
                winner_positions_open: 0,
                pairs_attempted: 0,
            }),
            settings,
        }
    }

    pub fn second_leg_delay_ms(&self) -> u64 {
        self.settings.second_leg_delay_ms
    }

    pub fn reserve(&self, planned_cost: Decimal) -> Result<PaperReservation<'_>> {
        let mut ledger = self.ledger.lock().expect("paper ledger lock poisoned");
        let available = ledger.cash_remaining - ledger.cash_reserved;
        if available < planned_cost {
            anyhow::bail!(
                "paper balance exhausted: available {}, planned {}",
                available,
                planned_cost
            );
        }
        ledger.cash_reserved += planned_cost;
        Ok(PaperReservation {
            engine: self,
            planned_cost,
            active: true,
        })
    }

    pub fn simulate_buy(
        &self,
        asks: &[OrderSummary],
        limit: Decimal,
        requested: Decimal,
        order_type: &OrderType,
    ) -> PaperFill {
        simulate_buy(
            asks,
            limit,
            requested,
            self.settings.liquidity_haircut,
            order_type,
        )
    }

    /// Apply persisted Late-Resolution Winner Trading history before new paper orders are admitted.
    pub fn restore_winner_history(
        &self,
        pending_cost: Decimal,
        total_spent: Decimal,
        realized_pnl: Decimal,
        open_positions: u64,
    ) -> PaperSnapshot {
        let mut ledger = self.ledger.lock().expect("paper ledger lock poisoned");
        ledger.cash_remaining = (ledger.cash_remaining + realized_pnl - pending_cost).max(dec!(0));
        ledger.total_spent += total_spent.max(dec!(0));
        ledger.unmatched_cost += pending_cost.max(dec!(0));
        ledger.winner_realized_pnl += realized_pnl;
        ledger.winner_positions_open += open_positions;
        snapshot(&self.settings, &ledger)
    }

    /// Resolve one open Late-Resolution Winner Trading position and release its locked paper cash.
    pub fn settle_winner(&self, cost: Decimal, payout: Decimal) -> PaperSnapshot {
        let mut ledger = self.ledger.lock().expect("paper ledger lock poisoned");
        let cost = cost.max(dec!(0));
        let payout = payout.max(dec!(0));
        ledger.cash_remaining += payout;
        ledger.unmatched_cost = (ledger.unmatched_cost - cost).max(dec!(0));
        ledger.winner_realized_pnl += payout - cost;
        ledger.winner_positions_open = ledger.winner_positions_open.saturating_sub(1);
        snapshot(&self.settings, &ledger)
    }

    #[cfg(test)]
    pub fn record_pair(&self, yes: PaperFill, no: PaperFill) -> PaperSnapshot {
        let mut ledger = self.ledger.lock().expect("paper ledger lock poisoned");
        record_fills(&mut ledger, yes, no);
        snapshot(&self.settings, &ledger)
    }
}

pub struct PaperReservation<'a> {
    engine: &'a PaperTradingEngine,
    planned_cost: Decimal,
    active: bool,
}

impl PaperReservation<'_> {
    pub fn commit(mut self, yes: PaperFill, no: PaperFill) -> PaperSnapshot {
        let mut ledger = self
            .engine
            .ledger
            .lock()
            .expect("paper ledger lock poisoned");
        ledger.cash_reserved = (ledger.cash_reserved - self.planned_cost).max(dec!(0));
        record_fills(&mut ledger, yes, no);
        self.active = false;
        snapshot(&self.engine.settings, &ledger)
    }

    pub fn commit_winner(mut self, fill: PaperFill) -> PaperSnapshot {
        let mut ledger = self
            .engine
            .ledger
            .lock()
            .expect("paper ledger lock poisoned");
        ledger.cash_reserved = (ledger.cash_reserved - self.planned_cost).max(dec!(0));
        if fill.filled > dec!(0) {
            ledger.cash_remaining = (ledger.cash_remaining - fill.cost).max(dec!(0));
            ledger.total_spent += fill.cost;
            ledger.unmatched_cost += fill.cost;
            ledger.winner_positions_open += 1;
        }
        self.active = false;
        snapshot(&self.engine.settings, &ledger)
    }
}

impl Drop for PaperReservation<'_> {
    fn drop(&mut self) {
        if self.active {
            let mut ledger = self
                .engine
                .ledger
                .lock()
                .expect("paper ledger lock poisoned");
            ledger.cash_reserved = (ledger.cash_reserved - self.planned_cost).max(dec!(0));
        }
    }
}

fn record_fills(ledger: &mut PaperLedger, yes: PaperFill, no: PaperFill) {
    let spent = yes.cost + no.cost;
    let paired = yes.filled.min(no.filled);
    // A matched YES+NO pair has a collateral claim of $1. Credit that value
    // immediately in the virtual ledger; unmatched legs remain locked at cost.
    ledger.cash_remaining = (ledger.cash_remaining - spent + paired).max(dec!(0));
    ledger.total_spent += spent;
    ledger.pairs_attempted += 1;

    let yes_fee_per_share = if yes.filled > dec!(0) {
        yes.fee / yes.filled
    } else {
        dec!(0)
    };
    let no_fee_per_share = if no.filled > dec!(0) {
        no.fee / no.filled
    } else {
        dec!(0)
    };
    let paired_cost = (yes.average_price + yes_fee_per_share) * paired
        + (no.average_price + no_fee_per_share) * paired;
    ledger.paired_shares += paired;
    ledger.theoretical_pnl += paired - paired_cost;
    ledger.unmatched_cost += (yes.filled - paired).max(dec!(0))
        * (yes.average_price + yes_fee_per_share)
        + (no.filled - paired).max(dec!(0)) * (no.average_price + no_fee_per_share);
}

fn snapshot(settings: &PaperSettings, ledger: &PaperLedger) -> PaperSnapshot {
    PaperSnapshot {
        initial_balance: settings.initial_balance,
        cash_remaining: ledger.cash_remaining,
        cash_reserved: ledger.cash_reserved,
        total_spent: ledger.total_spent,
        paired_shares: ledger.paired_shares,
        unmatched_cost: ledger.unmatched_cost,
        theoretical_pnl: ledger.theoretical_pnl,
        winner_realized_pnl: ledger.winner_realized_pnl,
        winner_positions_open: ledger.winner_positions_open,
        pairs_attempted: ledger.pairs_attempted,
    }
}

fn simulate_buy(
    asks: &[OrderSummary],
    limit: Decimal,
    requested: Decimal,
    liquidity_haircut: Decimal,
    order_type: &OrderType,
) -> PaperFill {
    let mut eligible: Vec<_> = asks
        .iter()
        .filter(|level| level.price <= limit && level.size > dec!(0))
        .collect();
    eligible.sort_by_key(|level| level.price);

    let available: Decimal = eligible
        .iter()
        .map(|level| level.size * liquidity_haircut)
        .sum();
    if matches!(order_type, OrderType::FOK) && available < requested {
        return PaperFill {
            requested,
            filled: dec!(0),
            cost: dec!(0),
            fee: dec!(0),
            average_price: dec!(0),
        };
    }

    let mut remaining = requested;
    let mut filled = dec!(0);
    let mut cost = dec!(0);
    for level in eligible {
        if remaining <= dec!(0) {
            break;
        }
        let level_available = level.size * liquidity_haircut;
        let take = remaining.min(level_available);
        filled += take;
        cost += take * level.price;
        remaining -= take;
    }
    let filled = (filled * dec!(100)).floor() / dec!(100);
    let cost = if filled > dec!(0) {
        // Recompute proportional cost after share precision truncation.
        cost * filled / (requested - remaining)
    } else {
        dec!(0)
    };
    PaperFill {
        requested,
        filled,
        cost,
        fee: dec!(0),
        average_price: if filled > dec!(0) {
            cost / filled
        } else {
            dec!(0)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: Decimal, size: Decimal) -> OrderSummary {
        OrderSummary::builder().price(price).size(size).build()
    }

    #[test]
    fn walks_live_depth_only_up_to_limit() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(100),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let fill = engine.simulate_buy(
            &[
                level(dec!(0.40), dec!(2)),
                level(dec!(0.41), dec!(3)),
                level(dec!(0.50), dec!(10)),
            ],
            dec!(0.41),
            dec!(4),
            &OrderType::FAK,
        );
        assert_eq!(fill.filled, dec!(4));
        assert_eq!(fill.cost, dec!(1.62));
        assert_eq!(fill.average_price, dec!(0.405));
    }

    #[test]
    fn fok_rejects_insufficient_haircut_depth_and_fak_partially_fills() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(100),
            liquidity_haircut: dec!(0.5),
            second_leg_delay_ms: 0,
        });
        let asks = [level(dec!(0.40), dec!(10))];
        assert_eq!(
            engine
                .simulate_buy(&asks, dec!(0.40), dec!(6), &OrderType::FOK)
                .filled,
            dec!(0)
        );
        assert_eq!(
            engine
                .simulate_buy(&asks, dec!(0.40), dec!(6), &OrderType::FAK)
                .filled,
            dec!(5)
        );
    }

    #[test]
    fn buy_simulation_does_not_mutate_the_paper_ledger() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(100),
            liquidity_haircut: dec!(0.75),
            second_leg_delay_ms: 0,
        });
        let before = {
            let ledger = engine.ledger.lock().unwrap();
            snapshot(&engine.settings, &ledger)
        };

        let fill = engine.simulate_buy(
            &[level(dec!(0.95), dec!(20))],
            dec!(0.95),
            dec!(10),
            &OrderType::FAK,
        );

        let after = {
            let ledger = engine.ledger.lock().unwrap();
            snapshot(&engine.settings, &ledger)
        };
        assert_eq!(fill.filled, dec!(10));
        assert_eq!(before, after);
    }

    #[test]
    fn ledger_tracks_virtual_cash_pnl_and_unmatched_cost() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(10),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let snapshot = engine.record_pair(
            PaperFill {
                requested: dec!(3),
                filled: dec!(3),
                cost: dec!(1.2),
                fee: dec!(0),
                average_price: dec!(0.4),
            },
            PaperFill {
                requested: dec!(3),
                filled: dec!(2),
                cost: dec!(1),
                fee: dec!(0),
                average_price: dec!(0.5),
            },
        );
        assert_eq!(snapshot.cash_remaining, dec!(9.8));
        assert_eq!(snapshot.theoretical_pnl, dec!(0.2));
        assert_eq!(snapshot.unmatched_cost, dec!(0.4));
    }

    #[test]
    fn ledger_subtracts_both_leg_dynamic_fees_from_pair_pnl() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(20),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let snapshot = engine.record_pair(
            PaperFill {
                requested: dec!(10),
                filled: dec!(10),
                cost: dec!(4.67325),
                fee: dec!(0.17325),
                average_price: dec!(0.45),
            },
            PaperFill {
                requested: dec!(10),
                filled: dec!(10),
                cost: dec!(5.175),
                fee: dec!(0.175),
                average_price: dec!(0.50),
            },
        );

        assert_eq!(snapshot.total_spent, dec!(9.84825));
        assert_eq!(snapshot.theoretical_pnl, dec!(0.15175));
        assert_eq!(snapshot.unmatched_cost, dec!(0));
    }

    #[test]
    fn reservations_prevent_concurrent_virtual_overspend_and_release_on_drop() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(5),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let reservation = engine.reserve(dec!(4)).unwrap();
        assert!(engine.reserve(dec!(2)).is_err());
        drop(reservation);
        assert!(engine.reserve(dec!(5)).is_ok());
    }

    #[test]
    fn winner_fill_locks_cash_until_resolution() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(20),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let reservation = engine.reserve(dec!(10)).unwrap();
        let filled = PaperFill {
            requested: dec!(10),
            filled: dec!(10),
            cost: dec!(9.6),
            fee: dec!(0),
            average_price: dec!(0.96),
        };
        let open = reservation.commit_winner(filled);
        assert_eq!(open.cash_remaining, dec!(10.4));
        assert_eq!(open.unmatched_cost, dec!(9.6));
        assert_eq!(open.winner_positions_open, 1);

        let settled = engine.settle_winner(dec!(9.6), dec!(10));
        assert_eq!(settled.cash_remaining, dec!(20.4));
        assert_eq!(settled.unmatched_cost, dec!(0));
        assert_eq!(settled.winner_realized_pnl, dec!(0.4));
        assert_eq!(settled.winner_positions_open, 0);
    }

    #[test]
    fn restored_winner_history_rebuilds_available_cash() {
        let engine = PaperTradingEngine::new(PaperSettings {
            initial_balance: dec!(100),
            liquidity_haircut: dec!(1),
            second_leg_delay_ms: 0,
        });
        let restored = engine.restore_winner_history(dec!(20), dec!(60), dec!(-5), 1);
        assert_eq!(restored.cash_remaining, dec!(75));
        assert_eq!(restored.total_spent, dec!(60));
        assert_eq!(restored.unmatched_cost, dec!(20));
        assert_eq!(restored.winner_realized_pnl, dec!(-5));
        assert_eq!(restored.winner_positions_open, 1);
    }
}
