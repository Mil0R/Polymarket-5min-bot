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

/// Decide whether the market's direction is decided enough to buy the winner.
///
/// Rules (window-end state):
/// - one side's best ask >= `min_price` (e.g. 0.95) AND
/// - the other side's best ask is absent or <= `1 - min_price`
///   (the losing side must not itself be expensive).
/// Returns `None` when direction is not (yet) decided.
pub fn detect_winner(
    yes_ask: Option<Decimal>,
    no_ask: Option<Decimal>,
    min_price: Decimal,
) -> Option<WinnerSignal> {
    let min = min_price.clamp(dec!(0.5), dec!(0.999));
    let max_loser = dec!(1) - min;

    let yes_decided = matches!(yes_ask, Some(p) if p >= min);
    let no_decided = matches!(no_ask, Some(p) if p >= min);
    // Both sides expensive (both guard-sided, e.g. 0.84/0.84) → not decided.
    if yes_decided && no_decided {
        return None;
    }

    if yes_decided {
        let loser_ok = no_ask.map_or(true, |p| p <= max_loser);
        if loser_ok {
            return Some(WinnerSignal {
                buy_yes: true,
                price: yes_ask.expect("checked above"),
            });
        }
    }
    if no_decided {
        let loser_ok = yes_ask.map_or(true, |p| p <= max_loser);
        if loser_ok {
            return Some(WinnerSignal {
                buy_yes: false,
                price: no_ask.expect("checked above"),
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
    fn buys_yes_when_yes_ask_is_decided_and_no_is_cheap() {
        let signal = detect_winner(Some(dec!(0.98)), Some(dec!(0.02)), dec!(0.95)).unwrap();
        assert!(signal.buy_yes);
        assert_eq!(signal.price, dec!(0.98));
    }

    #[test]
    fn buys_no_when_no_ask_is_decided_and_yes_is_cheap() {
        let signal = detect_winner(Some(dec!(0.03)), Some(dec!(0.98)), dec!(0.95)).unwrap();
        assert!(!signal.buy_yes);
        assert_eq!(signal.price, dec!(0.98));
    }

    #[test]
    fn buys_yes_when_no_side_has_no_asks() {
        // NO side has no asks at all; YES side decided → buy YES.
        let signal = detect_winner(Some(dec!(0.98)), None, dec!(0.95)).unwrap();
        assert!(signal.buy_yes);
        assert_eq!(signal.price, dec!(0.98));
    }

    #[test]
    fn rejects_when_both_sides_expensive() {
        // Classic guard-order shape 0.84/0.84: direction not decided.
        assert!(detect_winner(Some(dec!(0.84)), Some(dec!(0.84)), dec!(0.95)).is_none());
    }

    #[test]
    fn rejects_when_loser_side_still_expensive() {
        // YES at 0.98 but NO at 0.30 → not a clear winner (loser not cheap enough).
        assert!(detect_winner(Some(dec!(0.98)), Some(dec!(0.30)), dec!(0.95)).is_none());
        assert!(detect_winner(Some(dec!(0.98)), Some(dec!(0.04)), dec!(0.95)).is_some());
    }

    #[test]
    fn rejects_when_winner_side_below_min_price() {
        assert!(detect_winner(Some(dec!(0.80)), Some(dec!(0.10)), dec!(0.95)).is_none());
    }

    #[test]
    fn caps_size_by_notional() {
        assert_eq!(capped_size(dec!(0.98), dec!(50)), dec!(51.02));
        assert_eq!(capped_size(dec!(0.99), dec!(100)), dec!(101.01));
        assert_eq!(capped_size(dec!(0.98), dec!(0)), dec!(0));
    }
}
