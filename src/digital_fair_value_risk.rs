//! SDDFV-only virtual cash ledger and bounded, receive-time-aware reference audit.
//! Nothing here can access a signer or submit an order.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct PaperLedger {
    initial: f64,
    cash: f64,
    exposure_limit: f64,
    asset_limit: f64,
    positions: BTreeMap<String, (String, f64)>,
    settled: BTreeMap<String, f64>,
    high_water: f64,
    max_drawdown: f64,
}

impl PaperLedger {
    pub fn new(initial: f64, exposure_limit: f64, asset_limit: f64) -> Result<Self> {
        if [initial, exposure_limit, asset_limit]
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0)
            || exposure_limit > initial
            || asset_limit > exposure_limit
        {
            bail!("invalid SDDFV virtual budget/exposure limits");
        }
        Ok(Self {
            initial,
            cash: initial,
            exposure_limit,
            asset_limit,
            positions: BTreeMap::new(),
            settled: BTreeMap::new(),
            high_water: initial,
            max_drawdown: 0.0,
        })
    }

    pub fn reserve(&mut self, id: &str, asset: &str, cost: f64) -> Result<()> {
        if !cost.is_finite() || cost <= 0.0 || self.settled.contains_key(id) {
            bail!("invalid_or_reused_paper_reservation");
        }
        if let Some(previous) = self.positions.get(id) {
            if previous == &(asset.to_owned(), cost) {
                return Ok(());
            }
            bail!("conflicting_paper_reservation");
        }
        let used: f64 = self.positions.values().map(|p| p.1).sum();
        let asset_used: f64 = self
            .positions
            .values()
            .filter(|p| p.0 == asset)
            .map(|p| p.1)
            .sum();
        if cost > self.cash + 1e-9 {
            bail!("paper_cash_insufficient");
        }
        if used + cost > self.exposure_limit + 1e-9 {
            bail!("paper_total_exposure_exceeded");
        }
        if asset_used + cost > self.asset_limit + 1e-9 {
            bail!("paper_asset_exposure_exceeded");
        }
        self.cash -= cost;
        self.positions
            .insert(id.to_owned(), (asset.to_owned(), cost));
        Ok(())
    }

    pub fn settle(&mut self, id: &str, payout: f64) -> Result<()> {
        if !payout.is_finite() || payout < 0.0 {
            bail!("invalid_paper_payout");
        }
        if let Some(previous) = self.settled.get(id) {
            if (previous - payout).abs() < 1e-9 {
                return Ok(());
            }
            bail!("conflicting_paper_settlement");
        }
        if self.positions.remove(id).is_none() {
            bail!("paper_settlement_without_position");
        }
        self.cash += payout;
        self.settled.insert(id.to_owned(), payout);
        let nav = self.cash + self.positions.values().map(|p| p.1).sum::<f64>();
        self.high_water = self.high_water.max(nav);
        self.max_drawdown = self.max_drawdown.max(self.high_water - nav);
        Ok(())
    }

    pub fn cancel(&mut self, id: &str) {
        if let Some((_, cost)) = self.positions.remove(id) {
            self.cash += cost;
        }
    }

    pub fn snapshot(&self) -> Value {
        let locked: f64 = self.positions.values().map(|p| p.1).sum();
        json!({"initial_balance_usdc":self.initial,"available_cash_usdc":self.cash,
            "locked_cost_usdc":locked,"open_positions":self.positions.len(),
            "nav_cost_basis_usdc":self.cash+locked,
            "realized_pnl_usdc":self.cash+locked-self.initial,
            "max_realized_drawdown_usdc":self.max_drawdown,
            "max_total_exposure_usdc":self.exposure_limit,"max_asset_exposure_usdc":self.asset_limit,
            "valuation":"unresolved inventory at entry cost, NOT executable mark-to-market",
            "simulated_only":true})
    }
}

/// Two bounded receive versions per source-second. A concurrent later update
/// must not erase the already-known version of a checkpoint's source-second.
#[derive(Default)]
pub struct ReferenceHistory {
    buckets: BTreeMap<
        (String, String, i64),
        Vec<crate::late_resolution_winner_model::RtdsReferenceSample>,
    >,
}

impl ReferenceHistory {
    pub fn ingest(&mut self, sample: crate::late_resolution_winner_model::RtdsReferenceSample) {
        let cutoff = sample.received_at_ms.saturating_sub(120_000);
        self.buckets.retain(|_, versions| {
            versions.retain(|old| old.received_at_ms >= cutoff);
            !versions.is_empty()
        });
        let key = (
            sample.asset.clone(),
            sample.topic.clone(),
            sample.observed_at_ms / 1000,
        );
        let versions = self.buckets.entry(key).or_default();
        if !versions.iter().any(|old| *old == sample) {
            versions.push(sample);
        }
        versions.sort_by_key(|s| s.received_at_ms);
        while versions.len() > 2 {
            versions.remove(0);
        }
        // Absolute bound even if an upstream clock stops advancing.
        while self.buckets.len() > 1500 {
            let key = self
                .buckets
                .iter()
                .min_by_key(|(_, versions)| versions.last().unwrap().received_at_ms)
                .map(|(k, _)| k.clone())
                .unwrap();
            self.buckets.remove(&key);
        }
    }

    pub fn audit(&self, asset: &str, now_ms: i64, end_ms: i64) -> Value {
        let streams: Vec<Value> = [
            "crypto_prices_twap_sixty",
            "crypto_prices_chainlink",
            "crypto_prices",
        ]
        .into_iter()
        .map(|topic| {
            let samples: Vec<_> = self
                .buckets
                .values()
                .filter_map(|versions| versions.iter().filter(|s| {
                    s.asset == asset
                        && s.topic == topic
                        && s.observed_at_ms <= now_ms
                        && s.received_at_ms <= now_ms
                        && s.observed_at_ms >= now_ms - 120_000
                }).max_by_key(|s|s.received_at_ms))
                .collect();
            let seconds: std::collections::BTreeSet<i64>=samples.iter().map(|s|s.observed_at_ms/1000).collect();
            let coverage = |start: i64, end: i64| {
                let missing: Vec<_>=(start..end).filter(|second|!seconds.contains(second)).map(|second|second-start).collect();
                let expected=(end-start).max(0);
                json!({"expected_seconds":expected,"observed_seconds":expected-missing.len() as i64,
                    "missing_second_offsets":missing,"interpolated":false})
            };
            let latest=samples.iter().max_by_key(|s|s.observed_at_ms);
            let trailing=coverage(now_ms/1000-60,now_ms/1000);
            let final_start=end_ms/1000-60;
            json!({"topic":topic,"samples":samples,"coverage":{
                "trailing_60s":trailing,
                "trailing_proxy_eligible":trailing["observed_seconds"].as_i64().unwrap_or(0)>=54,
                "known_final_window":coverage(final_start,(now_ms/1000).min(end_ms/1000).max(final_start)),
                "latest_source_age_ms":latest.map(|s|now_ms-s.observed_at_ms),
                "latest_receive_age_ms":latest.map(|s|now_ms-s.received_at_ms),
                "decision_role":"coverage_audit_only"}})
        })
        .collect();
        json!({"schema_version":2,"bucket_ms":1000,"lookback_ms":120000,"asof_ms":now_ms,
            "window_end_ms":end_ms,"streams":streams,
            "aggregation_rule_verified":false,"decision_role":"research_only",
            "note":"Source-second buckets are audit inputs, not official TWAP weights. Do not substitute spot averages for settlement."})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cash_exposure_and_idempotent_settlement() {
        let mut l = PaperLedger::new(10.0, 10.0, 5.0).unwrap();
        l.reserve("a", "btc", 5.0).unwrap();
        assert!(l.reserve("b", "btc", 1.0).is_err());
        l.reserve("b", "eth", 5.0).unwrap();
        assert!(l.reserve("c", "sol", 1.0).is_err());
        l.settle("a", 0.0).unwrap();
        l.settle("a", 0.0).unwrap();
        assert!(l.settle("a", 2.0).is_err());
        assert!(l.reserve("a", "btc", 1.0).is_err());
        l.settle("b", 8.0).unwrap();
        assert_eq!(l.snapshot()["available_cash_usdc"], 8.0);
        assert_eq!(l.snapshot()["max_realized_drawdown_usdc"], 5.0);
        assert!(PaperLedger::new(f64::NAN, 5.0, 5.0).is_err());
    }
    #[test]
    fn reference_audit_is_bounded_and_past_only() {
        use crate::late_resolution_winner_model::RtdsReferenceSample;
        let mut h = ReferenceHistory::default();
        for i in 0..3000 {
            h.ingest(RtdsReferenceSample {
                topic: "crypto_prices_chainlink".into(),
                symbol: "btc/usd".into(),
                asset: "btc".into(),
                observed_at_ms: i * 1000,
                received_at_ms: i * 1000 + 50,
                publisher_at_ms: None,
                value: rust_decimal::Decimal::ONE,
            });
        }
        assert!(h.buckets.len() <= 121);
        let audit = h.audit("btc", 2_999_000, 3_030_000);
        for s in audit["streams"][1]["samples"].as_array().unwrap() {
            assert!(s["observed_at_ms"].as_i64().unwrap() <= 2_999_000);
            assert!(s["received_at_ms"].as_i64().unwrap() <= 2_999_000);
        }
        assert_eq!(audit["aggregation_rule_verified"], false);
    }
    #[test]
    fn late_same_second_receive_preserves_asof_version_and_reports_missing_seconds() {
        use crate::late_resolution_winner_model::RtdsReferenceSample;
        let mut h = ReferenceHistory::default();
        let sample = RtdsReferenceSample {
            topic: "crypto_prices_chainlink".into(),
            symbol: "btc/usd".into(),
            asset: "btc".into(),
            observed_at_ms: 1000,
            received_at_ms: 1100,
            publisher_at_ms: None,
            value: rust_decimal::Decimal::ONE,
        };
        h.ingest(sample.clone());
        h.ingest(RtdsReferenceSample {
            received_at_ms: 1500,
            ..sample
        });
        let audit = h.audit("btc", 1200, 31000);
        assert_eq!(audit["streams"][1]["samples"][0]["received_at_ms"], 1100);
        assert_eq!(
            audit["streams"][1]["coverage"]["trailing_60s"]["observed_seconds"],
            0
        );
        assert_eq!(
            audit["streams"][1]["coverage"]["trailing_proxy_eligible"],
            false
        );
        assert_eq!(
            audit["streams"][1]["coverage"]["trailing_60s"]["interpolated"],
            false
        );
    }
}
