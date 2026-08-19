//! Window-end "buy the determined winner" strategy.
//!
//! At the very end of an up/down window the direction is effectively decided:
//! the winning side's best ask sits at 0.95-0.99 (guard orders provide deep
//! liquidity there) while the losing side has no asks or sits near zero. Buying
//! the winner and holding to settlement (pays 1.00) earns the 1-5% edge with a
//! very high win rate. This is a single-leg directional buy — distinct from the
//! YES+NO arbitrage strategy.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Result of a winner-direction scan for one market.
#[derive(Debug, Clone, PartialEq)]
pub struct WinnerSignal {
    /// True = buy YES side, false = buy NO side.
    pub buy_yes: bool,
    /// Best ask of the winning side, used as the limit price.
    pub price: Decimal,
}

/// Decide whether the winner side's ask is buyable, from live REST book asks.
///
/// At window end the decided winner's side shows only high asks (0.95-0.99 —
/// makers selling the winner for small certainty gains) while the losing side
/// carries a full grid down to 0.01 (dumped/panic sells). We buy the side whose
/// best ask is high (`>= min_price`) and whose opponent is cheap (`< 0.1` or
/// absent) — direction inferred purely from Polymarket's own book, so it is
/// consistent with settlement.
pub fn detect_winner_asks(
    yes_ask: Option<Decimal>,
    no_ask: Option<Decimal>,
    min_price: Decimal,
    max_price: Decimal,
) -> Option<WinnerSignal> {
    let min = min_price.clamp(dec!(0.5), dec!(0.999));
    let max = max_price.clamp(min, dec!(0.9999));
    const LOSER_MAX: Decimal = dec!(0.1);

    let high_side = |ask: Option<Decimal>| match ask {
        Some(p) if p >= min && p <= max => Some(p),
        _ => None,
    };
    let low_or_absent = |ask: Option<Decimal>| ask.map_or(true, |p| p <= LOSER_MAX);

    if let Some(yes) = high_side(yes_ask) {
        if low_or_absent(no_ask) {
            return Some(WinnerSignal {
                buy_yes: true,
                price: yes,
            });
        }
    }
    if let Some(no) = high_side(no_ask) {
        if low_or_absent(yes_ask) {
            return Some(WinnerSignal {
                buy_yes: false,
                price: no,
            });
        }
    }
    None
}

/// Notional-capped share size for a buy at `price`, floored to 2 decimals.
pub fn capped_size(price: Decimal, max_order_usdc: Decimal) -> Decimal {
    if price <= dec!(0) {
        return dec!(0);
    }
    let raw = (max_order_usdc / price) * dec!(100);
    (raw.floor() / dec!(100)).max(dec!(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buys_yes_when_yes_ask_high_and_no_low() {
        let signal = detect_winner_asks(Some(dec!(0.98)), Some(dec!(0.01)), dec!(0.95), dec!(0.99)).unwrap();
        assert!(signal.buy_yes);
        assert_eq!(signal.price, dec!(0.98));
    }

    #[test]
    fn buys_no_when_no_ask_high_and_yes_low() {
        let signal = detect_winner_asks(Some(dec!(0.03)), Some(dec!(0.985)), dec!(0.95), dec!(0.99)).unwrap();
        assert!(!signal.buy_yes);
        assert_eq!(signal.price, dec!(0.985));
    }

    #[test]
    fn buys_yes_when_no_side_has_no_asks() {
        assert!(detect_winner_asks(Some(dec!(0.98)), None, dec!(0.95), dec!(0.99)).is_some());
    }

    #[test]
    fn rejects_when_both_sides_high() {
        // Direction not decided: both sides carry 0.99 guard asks.
        assert!(detect_winner_asks(Some(dec!(0.99)), Some(dec!(0.99)), dec!(0.95), dec!(0.99)).is_none());
    }

    #[test]
    fn rejects_when_winner_side_above_max_price() {
        assert!(detect_winner_asks(Some(dec!(0.993)), Some(dec!(0.01)), dec!(0.95), dec!(0.99)).is_none());
        assert!(detect_winner_asks(Some(dec!(0.985)), Some(dec!(0.01)), dec!(0.95), dec!(0.99)).is_some());
    }

    #[test]
    fn rejects_when_winner_below_min_price() {
        assert!(detect_winner_asks(Some(dec!(0.80)), Some(dec!(0.01)), dec!(0.95), dec!(0.99)).is_none());
    }

    #[test]
    fn caps_size_by_notional() {
        assert_eq!(capped_size(dec!(0.98), dec!(50)), dec!(51.02));
        assert_eq!(capped_size(dec!(0.99), dec!(100)), dec!(101.01));
        assert_eq!(capped_size(dec!(0.98), dec!(0)), dec!(0));
    }
}
