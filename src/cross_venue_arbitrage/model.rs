use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub fn number(v: &Value) -> Result<f64> {
    let n = v
        .as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .context("missing numeric value")?;
    if !n.is_finite() {
        bail!("non-finite value");
    }
    Ok(n)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Level {
    pub price: f64,
    pub size: f64,
}

pub fn levels(v: &Value, array: bool, complement: bool) -> Result<Vec<Level>> {
    let mut result = Vec::new();
    for item in v.as_array().context("missing orderbook side")? {
        let p = number(if array { &item[0] } else { &item["price"] })?;
        let size = number(if array { &item[1] } else { &item["size"] })?;
        if !(0.0..=1.0).contains(&p) || size < 0. {
            bail!("invalid book level");
        }
        if size > 0. {
            result.push(Level {
                price: if complement { 1. - p } else { p },
                size,
            });
        }
    }
    // Merge duplicate price levels before applying depth haircuts.
    result.sort_by(|a, b| a.price.total_cmp(&b.price));
    let mut merged: Vec<Level> = Vec::new();
    for item in result {
        if let Some(last) = merged.last_mut().filter(|last| last.price == item.price) {
            last.size += item.size;
        } else {
            merged.push(item);
        }
    }
    Ok(merged)
}

pub fn walk(levels: &[Level], quantity: f64, haircut: f64, buy: bool) -> Result<f64> {
    if !quantity.is_finite()
        || quantity <= 0.
        || !haircut.is_finite()
        || !(0.0..=1.0).contains(&haircut)
        || haircut == 0.
    {
        bail!("invalid size or haircut");
    }
    let mut sorted = levels.to_vec();
    sorted.sort_by(|a, b| {
        if buy {
            a.price.total_cmp(&b.price)
        } else {
            b.price.total_cmp(&a.price)
        }
    });
    let mut remaining = quantity;
    let mut value = 0.;
    for level in sorted {
        let take = remaining.min(level.size * haircut);
        value += take * level.price;
        remaining -= take;
        if remaining < 1e-8 {
            return Ok(value);
        }
    }
    bail!("insufficient_depth")
}

#[derive(Clone, Debug, Serialize)]
pub struct Quote {
    pub direction: String,
    pub quantity: f64,
    pub poly_cost: f64,
    pub kalshi_cost: f64,
    pub fee_reserve: f64,
    pub slippage_reserve: f64,
    pub net_profit: f64,
    pub net_per_pair: f64,
    pub capital: f64,
}

pub struct Pricing {
    pub haircut: f64,
    pub slippage_bps: f64,
    pub poly_fee_cap: f64,
    pub kalshi_fee_cap: f64,
    pub per_venue_budget: f64,
}

pub fn quote(
    direction: &str,
    poly: &[Level],
    kalshi: &[Level],
    q: f64,
    p: &Pricing,
) -> Result<Quote> {
    let pc = walk(poly, q, p.haircut, true)?;
    let kc = walk(kalshi, q, p.haircut, true)?;
    // Explicit reviewed per-contract cash-equivalent upper bounds; never infer zero fees.
    // Kalshi fee reserve rounds up to cents for this hypothetical order.
    let pf = q * p.poly_fee_cap;
    let kf = (q * p.kalshi_fee_cap * 100. - 1e-9).ceil() / 100.;
    let slippage = (pc + kc) * p.slippage_bps / 10000.;
    if pc * (1. + p.slippage_bps / 10000.) + pf > p.per_venue_budget
        || kc * (1. + p.slippage_bps / 10000.) + kf > p.per_venue_budget
    {
        bail!("venue_budget");
    }
    let capital = pc + kc + pf + kf + slippage;
    Ok(Quote {
        direction: direction.into(),
        quantity: q,
        poly_cost: pc,
        kalshi_cost: kc,
        fee_reserve: pf + kf,
        slippage_reserve: slippage,
        net_profit: q - capital,
        net_per_pair: (q - capital) / q,
        capital,
    })
}

pub fn stress(initial: &Quote, repeated: &Quote) -> Value {
    let q = initial.quantity;
    json!({"poly_first_locked_cost_pnl":q-initial.poly_cost-repeated.kalshi_cost-initial.fee_reserve.max(repeated.fee_reserve)-initial.slippage_reserve.max(repeated.slippage_reserve),
        "kalshi_first_locked_cost_pnl":q-initial.kalshi_cost-repeated.poly_cost-initial.fee_reserve.max(repeated.fee_reserve)-initial.slippage_reserve.max(repeated.slippage_reserve),
        "single_leg_zero_recovery_loss":-(initial.poly_cost.max(initial.kalshi_cost)+initial.fee_reserve+initial.slippage_reserve),
        "assumption":"equal NET payout quantities; fee caps must cover fees charged in shares; hypothetical, not fills"})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complement_uses_opposite_bids_with_depth_and_wire_order() {
        let asks = levels(&json!([["0.40", "2"], ["0.60", "3"]]), true, true).unwrap();
        assert!((walk(&asks, 4., 1., true).unwrap() - 1.8).abs() < 1e-9);
        assert!(walk(&asks, 4., 0.5, true).is_err());
    }
    #[test]
    fn costs_and_budget_remove_apparent_profit() {
        let book = vec![Level {
            price: 0.49,
            size: 100.,
        }];
        let p = Pricing {
            haircut: 1.,
            slippage_bps: 10.,
            poly_fee_cap: 0.01,
            kalshi_fee_cap: 0.0175,
            per_venue_budget: 100.,
        };
        assert!(quote("x", &book, &book, 5., &p).unwrap().net_profit < 0.);
        let p = Pricing {
            per_venue_budget: 1.,
            ..p
        };
        assert!(quote("x", &book, &book, 5., &p).is_err());
    }
    #[test]
    fn malformed_and_missing_books_fail_closed() {
        assert!(levels(&Value::Null, true, true).is_err());
        assert!(levels(&json!([["1.1", "3"]]), true, true).is_err());
        assert!(levels(&json!([["0.2", "NaN"]]), true, true).is_err());
    }
}
