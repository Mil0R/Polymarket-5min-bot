//! Standalone, read-only due diligence for explicitly supplied Polymarket wallets.
//!
//! The auditor reuses the candidate scout's profitability/risk/execution-fit
//! evaluation, then adds reward-dependency, cross-wallet timing/transaction
//! correlation, and sampled on-chain maker/taker counterparty evidence. Every
//! run is appended as one immutable NDJSON record.

use super::feed::{DATA_API_URL, GAMMA_API_URL};
use super::scout::{CandidateEvaluation, CandidateScout, ScoutConfig, ScoutProfile};
use alloy::primitives::keccak256;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

const PAGE_SIZE: usize = 500;
const HISTORY_CAP: usize = 10_000;
const CTF_EXCHANGE: &str = "0xe111180000d2663c0091e4f400237545b87b996b";
const NEG_RISK_EXCHANGE: &str = "0xe2222d279d744050d28e00520010520000310f59";

#[derive(Debug, Clone)]
pub struct WalletAuditConfig {
    pub addresses: Vec<String>,
    pub profile: ScoutProfile,
    pub lookback_days: i64,
    pub synchronization_window_secs: i64,
    pub onchain_receipt_sample: usize,
    pub polygon_rpc_url: Option<String>,
    pub output_file: PathBuf,
}

impl Default for WalletAuditConfig {
    fn default() -> Self {
        Self {
            addresses: Vec::new(),
            profile: ScoutProfile::Conservative,
            lookback_days: 90,
            synchronization_window_secs: 15,
            onchain_receipt_sample: 200,
            polygon_rpc_url: std::env::var("WALLET_COPY_TRADING_ONCHAIN_RPC_URL")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            output_file: PathBuf::from("results/wallet-copy-audits/audits.ndjson"),
        }
    }
}

impl WalletAuditConfig {
    pub fn validate(&self) -> Result<()> {
        if self.addresses.is_empty() {
            anyhow::bail!("at least one wallet address is required");
        }
        if self.addresses.len() > 50 {
            anyhow::bail!("at most 50 wallet addresses may be audited at once");
        }
        if !(1..=3650).contains(&self.lookback_days) {
            anyhow::bail!("lookback-days must be in 1..=3650");
        }
        if !(0..=3600).contains(&self.synchronization_window_secs) {
            anyhow::bail!("sync-window-secs must be in 0..=3600");
        }
        if self.onchain_receipt_sample > 5000 {
            anyhow::bail!("onchain-sample must be in 0..=5000");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletAuditReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub profile: ScoutProfile,
    pub lookback_days: i64,
    pub synchronization_window_secs: i64,
    pub methodology: AuditMethodology,
    pub address_resolutions: Vec<AddressResolution>,
    pub wallets: Vec<WalletAuditConclusion>,
    pub address_links: Vec<AddressLinkEvidence>,
    pub partial_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressResolution {
    pub input_address: String,
    pub audit_address: String,
    pub proxy_wallet_resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditMethodology {
    pub historical_delay_pnl_status: String,
    pub counterparty_status: String,
    pub funding_cluster_status: String,
    pub important_limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletAuditConclusion {
    pub address: String,
    pub verdict: String,
    pub worth_paper_copying: bool,
    pub overall_score: f64,
    pub integrity_risk_score: f64,
    pub scout: Option<CandidateEvaluation>,
    pub public_trades: usize,
    pub distinct_trade_transactions: usize,
    pub distinct_markets: usize,
    pub observable_trade_volume_usdc: f64,
    pub rewards_and_rebates_usdc: f64,
    pub reward_dependency_ratio: f64,
    pub trade_history_truncated: bool,
    pub activity_history_truncated: bool,
    pub onchain_receipts_requested: usize,
    pub onchain_receipts_loaded: usize,
    pub onchain_order_fills_matched: usize,
    pub counterparty_fill_count_hhi: Option<f64>,
    pub top_counterparties: Vec<CounterpartyEvidence>,
    pub hard_rejection_reasons: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterpartyEvidence {
    pub address: String,
    pub fill_count: usize,
    pub fill_count_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressLinkEvidence {
    pub address_a: String,
    pub address_b: String,
    pub shared_transaction_hashes: usize,
    pub shared_transaction_ratio: f64,
    pub synchronized_same_direction_trades: usize,
    pub synchronized_opposite_direction_trades: usize,
    pub direct_onchain_fill_links: usize,
    pub suspicion_score: f64,
    pub classification: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone)]
struct AuditTrade {
    condition_id: String,
    asset: String,
    side: String,
    size: f64,
    price: f64,
    timestamp: i64,
    transaction_hash: String,
}

#[derive(Debug, Clone, Default)]
struct ActivityStats {
    rewards_usdc: f64,
    truncated: bool,
}

#[derive(Debug, Clone, Default)]
struct OnchainEvidence {
    requested: usize,
    loaded: usize,
    matched_order_fills: usize,
    counterparties: HashMap<String, usize>,
    failures: Vec<String>,
}

#[derive(Debug, Clone)]
struct WalletRawEvidence {
    address: String,
    trades: Vec<AuditTrade>,
    trades_truncated: bool,
    activity: ActivityStats,
    onchain: OnchainEvidence,
}

pub struct WalletAuditor {
    config: WalletAuditConfig,
    http: reqwest::Client,
}

impl WalletAuditor {
    pub fn new(mut config: WalletAuditConfig) -> Result<Self> {
        config.validate()?;
        config.addresses = normalize_addresses(&config.addresses)?;
        Ok(Self {
            config,
            http: reqwest::Client::builder()
                .user_agent("polypulse-wallet-copy-audit")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("failed to construct wallet audit HTTP client")?,
        })
    }

    pub async fn run(&self) -> Result<WalletAuditReport> {
        let generated_at = Utc::now();
        let cutoff = generated_at - Duration::days(self.config.lookback_days);
        let (audit_addresses, address_resolutions, mut partial_failures) =
            self.resolve_profile_wallets().await;
        let mut scout_config = match self.config.profile {
            ScoutProfile::Conservative => ScoutConfig::conservative(),
            ScoutProfile::Research => ScoutConfig::research(),
        };
        scout_config.categories.clear();
        scout_config.subcategories.clear();
        scout_config.explicit_wallets = audit_addresses.clone();
        scout_config.max_candidates = audit_addresses.len();
        scout_config.top = audit_addresses.len();
        scout_config.lookback_days = self.config.lookback_days;
        scout_config.recent_days = self.config.lookback_days.min(30);
        let scout_report = CandidateScout::new(scout_config)?
            .run_without_outputs()
            .await?;
        let scout_by_address: HashMap<_, _> = scout_report
            .candidates
            .into_iter()
            .map(|candidate| (candidate.address.to_ascii_lowercase(), candidate))
            .collect();

        let mut raw = Vec::new();
        partial_failures.extend(scout_report.partial_failures);
        for address in &audit_addresses {
            match self.collect_wallet_evidence(address, cutoff).await {
                Ok(evidence) => raw.push(evidence),
                Err(error) => {
                    partial_failures
                        .push(format!("{address}: raw audit evidence failed: {error:#}"));
                    raw.push(WalletRawEvidence {
                        address: address.clone(),
                        trades: Vec::new(),
                        trades_truncated: false,
                        activity: ActivityStats::default(),
                        onchain: OnchainEvidence::default(),
                    });
                }
            }
        }

        let address_links = build_address_links(&raw, self.config.synchronization_window_secs);
        let mut wallets = Vec::new();
        for evidence in &raw {
            let candidate = scout_by_address
                .get(&evidence.address.to_ascii_lowercase())
                .cloned();
            wallets.push(build_conclusion(candidate, evidence, &address_links));
            partial_failures.extend(
                evidence
                    .onchain
                    .failures
                    .iter()
                    .map(|failure| format!("{}: {failure}", evidence.address)),
            );
        }
        wallets.sort_by(|left, right| {
            right
                .overall_score
                .partial_cmp(&left.overall_score)
                .unwrap_or(Ordering::Equal)
        });

        let counterparty_status = if self.config.onchain_receipt_sample == 0 {
            "disabled".to_string()
        } else if self.config.polygon_rpc_url.is_none() {
            "unavailable_no_polygon_rpc_url".to_string()
        } else if raw.iter().any(|wallet| wallet.onchain.loaded > 0) {
            "sampled_v2_order_filled_receipts".to_string()
        } else {
            "unavailable_rpc_or_receipt_failures".to_string()
        };
        let report = WalletAuditReport {
            schema_version: 1,
            generated_at,
            profile: self.config.profile,
            lookback_days: self.config.lookback_days,
            synchronization_window_secs: self.config.synchronization_window_secs,
            methodology: AuditMethodology {
                historical_delay_pnl_status: "not_measured_no_historical_executable_orderbooks; scout.live_execution_fit_is_current_book_proxy".into(),
                counterparty_status,
                funding_cluster_status: "not_measured_public_data_does_not_prove_common_wallet_control".into(),
                important_limitations: vec![
                    "A shared transaction, synchronized trade, or concentrated counterparty is evidence, not proof of common control.".into(),
                    "V2 OrderFilled receipt sampling does not cover every legacy contract or every historical fill.".into(),
                    "Reward dependency uses observable activity amounts and is kept separate from trading PnL.".into(),
                    "A PAPER_CANDIDATE verdict requires dry-run validation before any live use.".into(),
                ],
            },
            address_resolutions,
            wallets,
            address_links,
            partial_failures,
        };
        self.append_report(&report)?;
        Ok(report)
    }

    async fn resolve_profile_wallets(&self) -> (Vec<String>, Vec<AddressResolution>, Vec<String>) {
        let mut audit_addresses = BTreeSet::new();
        let mut resolutions = Vec::new();
        let mut failures = Vec::new();
        for input in &self.config.addresses {
            let url = format!("{GAMMA_API_URL}/public-profile?address={input}");
            let resolved = match self.http.get(url).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Value>().await {
                        Ok(profile) => profile
                            .get("proxyWallet")
                            .and_then(Value::as_str)
                            .map(str::to_ascii_lowercase)
                            .filter(|address| {
                                normalize_addresses(std::slice::from_ref(address)).is_ok()
                            }),
                        Err(error) => {
                            failures.push(format!(
                            "{input}: public profile response decode failed; using input address: {error}"
                        ));
                            None
                        }
                    }
                }
                Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => None,
                Ok(response) => {
                    failures.push(format!(
                        "{input}: public profile lookup returned {}; using input address",
                        response.status()
                    ));
                    None
                }
                Err(error) => {
                    failures.push(format!(
                        "{input}: public profile lookup failed; using input address: {error}"
                    ));
                    None
                }
            };
            let audit_address = resolved.unwrap_or_else(|| input.clone());
            audit_addresses.insert(audit_address.clone());
            resolutions.push(AddressResolution {
                input_address: input.clone(),
                proxy_wallet_resolved: audit_address != *input,
                audit_address,
            });
        }
        (audit_addresses.into_iter().collect(), resolutions, failures)
    }

    async fn collect_wallet_evidence(
        &self,
        address: &str,
        cutoff: DateTime<Utc>,
    ) -> Result<WalletRawEvidence> {
        let (trades, trades_truncated) = self.fetch_trades(address, cutoff.timestamp()).await?;
        let activity = self.fetch_activity(address, cutoff.timestamp()).await?;
        let onchain = self.inspect_counterparties(address, &trades).await;
        Ok(WalletRawEvidence {
            address: address.into(),
            trades,
            trades_truncated,
            activity,
            onchain,
        })
    }

    async fn fetch_trades(&self, address: &str, cutoff: i64) -> Result<(Vec<AuditTrade>, bool)> {
        let mut trades = Vec::new();
        let mut truncated = false;
        for offset in (0..HISTORY_CAP).step_by(PAGE_SIZE) {
            let url = format!(
                "{DATA_API_URL}/trades?user={address}&limit={PAGE_SIZE}&offset={offset}&takerOnly=false"
            );
            let rows = self
                .http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let rows = rows.as_array().context("trades response is not an array")?;
            let mut reached_cutoff = false;
            for row in rows {
                let Some(trade) = parse_trade(row) else {
                    continue;
                };
                if trade.timestamp < cutoff {
                    reached_cutoff = true;
                    continue;
                }
                trades.push(trade);
            }
            if rows.len() < PAGE_SIZE || reached_cutoff {
                break;
            }
            if offset + PAGE_SIZE == HISTORY_CAP {
                truncated = true;
            }
        }
        let mut seen = HashSet::new();
        trades.retain(|trade| {
            seen.insert(format!(
                "{}:{}:{}:{}:{}:{}",
                trade.transaction_hash,
                trade.condition_id,
                trade.asset,
                trade.side,
                trade.timestamp,
                trade.size
            ))
        });
        trades.sort_by_key(|trade| trade.timestamp);
        Ok((trades, truncated))
    }

    async fn fetch_activity(&self, address: &str, cutoff: i64) -> Result<ActivityStats> {
        let mut stats = ActivityStats::default();
        for offset in (0..HISTORY_CAP).step_by(PAGE_SIZE) {
            let url = format!(
                "{DATA_API_URL}/activity?user={address}&limit={PAGE_SIZE}&offset={offset}&start={cutoff}&sortDirection=DESC"
            );
            let rows = self
                .http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let rows = rows
                .as_array()
                .context("activity response is not an array")?;
            for row in rows {
                let activity_type = row
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                if matches!(
                    activity_type.as_str(),
                    "REWARD" | "MAKER_REBATE" | "REFERRAL_REWARD"
                ) {
                    stats.rewards_usdc += value_f64(row.get("usdcSize"))
                        .or_else(|| value_f64(row.get("size")))
                        .unwrap_or_default()
                        .abs();
                }
            }
            if rows.len() < PAGE_SIZE {
                break;
            }
            if offset + PAGE_SIZE == HISTORY_CAP {
                stats.truncated = true;
            }
        }
        Ok(stats)
    }

    async fn inspect_counterparties(
        &self,
        address: &str,
        trades: &[AuditTrade],
    ) -> OnchainEvidence {
        let Some(rpc_url) = self.config.polygon_rpc_url.as_deref() else {
            return OnchainEvidence::default();
        };
        if self.config.onchain_receipt_sample == 0 {
            return OnchainEvidence::default();
        }
        let mut seen_hashes = HashSet::new();
        let hashes = trades
            .iter()
            .rev()
            .map(|trade| trade.transaction_hash.to_ascii_lowercase())
            .filter(|hash| {
                hash.starts_with("0x") && hash.len() == 66 && seen_hashes.insert(hash.clone())
            })
            .take(self.config.onchain_receipt_sample)
            .collect::<Vec<_>>();
        let requested = hashes.len();
        let responses = stream::iter(hashes.into_iter().enumerate())
            .map(|(index, hash)| async move {
                let response = self
                    .http
                    .post(rpc_url)
                    .json(&json!({
                        "jsonrpc": "2.0",
                        "id": index + 1,
                        "method": "eth_getTransactionReceipt",
                        "params": [hash],
                    }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await?;
                Ok::<_, anyhow::Error>((hash, response))
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await;

        let target = address.to_ascii_lowercase();
        let mut evidence = OnchainEvidence {
            requested,
            ..OnchainEvidence::default()
        };
        for response in responses {
            match response {
                Ok((hash, value)) => {
                    let Some(receipt) = value.get("result").filter(|result| !result.is_null())
                    else {
                        if evidence.failures.len() < 20 {
                            evidence
                                .failures
                                .push(format!("receipt unavailable for {hash}"));
                        }
                        continue;
                    };
                    evidence.loaded += 1;
                    let (matched, counterparties) = counterparties_from_receipt(receipt, &target);
                    evidence.matched_order_fills += matched;
                    for counterparty in counterparties {
                        *evidence.counterparties.entry(counterparty).or_default() += 1;
                    }
                }
                Err(error) => {
                    if evidence.failures.len() < 20 {
                        evidence
                            .failures
                            .push(format!("receipt RPC request failed: {error}"));
                    }
                }
            }
        }
        evidence
    }

    fn append_report(&self, report: &WalletAuditReport) -> Result<()> {
        if let Some(parent) = self
            .config
            .output_file
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.config.output_file)
            .with_context(|| {
                format!(
                    "failed to open audit output {}",
                    self.config.output_file.display()
                )
            })?;
        file.lock_exclusive()?;
        writeln!(file, "{}", serde_json::to_string(report)?)?;
        file.flush()?;
        file.unlock()?;
        Ok(())
    }
}

fn normalize_addresses(addresses: &[String]) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for address in addresses {
        let value = address.trim().to_ascii_lowercase();
        if value.len() != 42
            || !value.starts_with("0x")
            || !value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("invalid wallet address '{address}'");
        }
        normalized.insert(value);
    }
    Ok(normalized.into_iter().collect())
}

fn parse_trade(row: &Value) -> Option<AuditTrade> {
    let timestamp = row.get("timestamp")?.as_i64()?;
    Some(AuditTrade {
        condition_id: row.get("conditionId")?.as_str()?.to_ascii_lowercase(),
        asset: value_string(row.get("asset")?)?,
        side: row
            .get("side")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase(),
        size: value_f64(row.get("size"))?,
        price: value_f64(row.get("price"))?,
        timestamp,
        transaction_hash: row
            .get("transactionHash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase(),
    })
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn value_f64(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().or_else(|| value?.as_str()?.parse().ok())
}

fn counterparties_from_receipt(receipt: &Value, target: &str) -> (usize, Vec<String>) {
    let event_topic = format!(
        "{:#x}",
        keccak256(b"OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")
    );
    let mut matched = 0;
    let mut counterparties = Vec::new();
    let Some(logs) = receipt.get("logs").and_then(Value::as_array) else {
        return (0, counterparties);
    };
    for log in logs {
        let contract = log
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !is_exchange(&contract) {
            continue;
        }
        let Some(topics) = log.get("topics").and_then(Value::as_array) else {
            continue;
        };
        if topics.len() < 4
            || !topics[0]
                .as_str()
                .is_some_and(|topic| topic.eq_ignore_ascii_case(&event_topic))
        {
            continue;
        }
        let Some(maker) = topic_address(&topics[2]) else {
            continue;
        };
        let Some(taker) = topic_address(&topics[3]) else {
            continue;
        };
        if maker == target || taker == target {
            matched += 1;
        }
        if taker == target && maker != target && !is_exchange(&maker) {
            counterparties.push(maker);
        } else if maker == target && taker != target && !is_exchange(&taker) {
            counterparties.push(taker);
        }
    }
    (matched, counterparties)
}

fn topic_address(value: &Value) -> Option<String> {
    let topic = value.as_str()?.trim_start_matches("0x");
    if topic.len() != 64 || !topic.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", &topic[24..]).to_ascii_lowercase())
}

fn is_exchange(address: &str) -> bool {
    address.eq_ignore_ascii_case(CTF_EXCHANGE) || address.eq_ignore_ascii_case(NEG_RISK_EXCHANGE)
}

fn build_address_links(
    wallets: &[WalletRawEvidence],
    synchronization_window_secs: i64,
) -> Vec<AddressLinkEvidence> {
    let mut links = Vec::new();
    for left_index in 0..wallets.len() {
        for right_index in (left_index + 1)..wallets.len() {
            let left = &wallets[left_index];
            let right = &wallets[right_index];
            let left_hashes = transaction_hashes(&left.trades);
            let right_hashes = transaction_hashes(&right.trades);
            let shared_transaction_hashes = left_hashes.intersection(&right_hashes).count();
            let shared_transaction_ratio = ratio(
                shared_transaction_hashes,
                left_hashes.len().min(right_hashes.len()),
            );
            let (same_direction, opposite_direction) =
                synchronized_trade_counts(&left.trades, &right.trades, synchronization_window_secs);
            let direct_onchain_fill_links = left
                .onchain
                .counterparties
                .get(&right.address)
                .copied()
                .unwrap_or_default()
                .saturating_add(
                    right
                        .onchain
                        .counterparties
                        .get(&left.address)
                        .copied()
                        .unwrap_or_default(),
                );
            let activity_denominator = left.trades.len().min(right.trades.len()).max(1);
            let opposite_ratio = opposite_direction as f64 / activity_denominator as f64;
            let same_ratio = same_direction as f64 / activity_denominator as f64;
            let suspicion_score = round2(
                100.0
                    * (0.45 * shared_transaction_ratio.min(1.0)
                        + 0.35 * opposite_ratio.min(1.0)
                        + 0.10 * same_ratio.min(1.0)
                        + 0.10 * (direct_onchain_fill_links as f64 / 10.0).min(1.0)),
            );
            let mut evidence = Vec::new();
            if shared_transaction_hashes > 0 {
                evidence.push(format!(
                    "shared_transaction_hashes={shared_transaction_hashes}"
                ));
            }
            if direct_onchain_fill_links > 0 {
                evidence.push(format!(
                    "direct_onchain_fill_links={direct_onchain_fill_links}"
                ));
            }
            if opposite_direction > 0 {
                evidence.push(format!(
                    "synchronized_opposite_direction={opposite_direction}"
                ));
            }
            if same_direction > 0 {
                evidence.push(format!("synchronized_same_direction={same_direction}"));
            }
            let classification = if direct_onchain_fill_links >= 3
                && (shared_transaction_hashes >= 3 || opposite_direction >= 3)
            {
                "HIGH_SUSPICION"
            } else if suspicion_score >= 25.0 || direct_onchain_fill_links > 0 {
                "REVIEW"
            } else {
                "NO_STRONG_LINK_FOUND"
            };
            links.push(AddressLinkEvidence {
                address_a: left.address.clone(),
                address_b: right.address.clone(),
                shared_transaction_hashes,
                shared_transaction_ratio: round4(shared_transaction_ratio),
                synchronized_same_direction_trades: same_direction,
                synchronized_opposite_direction_trades: opposite_direction,
                direct_onchain_fill_links,
                suspicion_score,
                classification: classification.into(),
                evidence,
            });
        }
    }
    links
}

fn transaction_hashes(trades: &[AuditTrade]) -> HashSet<String> {
    trades
        .iter()
        .map(|trade| trade.transaction_hash.clone())
        .filter(|hash| !hash.is_empty())
        .collect()
}

fn synchronized_trade_counts(
    left: &[AuditTrade],
    right: &[AuditTrade],
    window_secs: i64,
) -> (usize, usize) {
    let mut right_by_market: HashMap<&str, Vec<&AuditTrade>> = HashMap::new();
    for trade in right {
        right_by_market
            .entry(&trade.condition_id)
            .or_default()
            .push(trade);
    }
    let mut same = 0;
    let mut opposite = 0;
    for trade in left {
        let Some(candidates) = right_by_market.get(trade.condition_id.as_str()) else {
            continue;
        };
        let nearest = candidates
            .iter()
            .filter(|other| (trade.timestamp - other.timestamp).abs() <= window_secs)
            .min_by_key(|other| (trade.timestamp - other.timestamp).abs());
        if let Some(other) = nearest {
            if trade.side == other.side {
                same += 1;
            } else {
                opposite += 1;
            }
        }
    }
    (same, opposite)
}

fn build_conclusion(
    candidate: Option<CandidateEvaluation>,
    evidence: &WalletRawEvidence,
    links: &[AddressLinkEvidence],
) -> WalletAuditConclusion {
    let trade_volume = evidence
        .trades
        .iter()
        .map(|trade| (trade.size * trade.price).abs())
        .sum::<f64>();
    let distinct_transactions = transaction_hashes(&evidence.trades).len();
    let distinct_markets = evidence
        .trades
        .iter()
        .map(|trade| trade.condition_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    let total_pnl = candidate
        .as_ref()
        .map(|candidate| candidate.total_pnl_usdc.abs())
        .unwrap_or_default();
    let reward_dependency_ratio = if total_pnl + evidence.activity.rewards_usdc > 0.0 {
        evidence.activity.rewards_usdc / (total_pnl + evidence.activity.rewards_usdc)
    } else {
        0.0
    };

    let total_counterparty_fills = evidence.onchain.counterparties.values().sum::<usize>();
    let counterparty_hhi = (total_counterparty_fills > 0).then(|| {
        evidence
            .onchain
            .counterparties
            .values()
            .map(|count| (*count as f64 / total_counterparty_fills as f64).powi(2))
            .sum::<f64>()
    });
    let mut top_counterparties = evidence
        .onchain
        .counterparties
        .iter()
        .map(|(address, count)| CounterpartyEvidence {
            address: address.clone(),
            fill_count: *count,
            fill_count_ratio: round4(ratio(*count, total_counterparty_fills)),
        })
        .collect::<Vec<_>>();
    top_counterparties.sort_by_key(|entry| std::cmp::Reverse(entry.fill_count));
    top_counterparties.truncate(10);

    let linked_risk = links
        .iter()
        .filter(|link| link.address_a == evidence.address || link.address_b == evidence.address)
        .map(|link| link.suspicion_score)
        .fold(0.0_f64, f64::max);
    let mut integrity_risk = linked_risk;
    if reward_dependency_ratio > 0.50 {
        integrity_risk += 20.0;
    } else if reward_dependency_ratio > 0.25 {
        integrity_risk += 10.0;
    }
    if counterparty_hhi.is_some_and(|hhi| hhi > 0.50) && total_counterparty_fills >= 10 {
        integrity_risk += 10.0;
    }
    integrity_risk = round2(integrity_risk.min(100.0));

    let mut hard_rejection_reasons = Vec::new();
    let mut warnings = Vec::new();
    if candidate.is_none() {
        hard_rejection_reasons.push("candidate_evaluation_unavailable".into());
    } else if let Some(candidate) = &candidate {
        hard_rejection_reasons.extend(candidate.rejection_reasons.clone());
        warnings.extend(candidate.warnings.clone());
    }
    if integrity_risk >= 50.0 {
        hard_rejection_reasons.push(format!(
            "integrity_risk_too_high: {:.2} >= 50",
            integrity_risk
        ));
    }
    if reward_dependency_ratio > 0.50 {
        hard_rejection_reasons.push(format!(
            "reward_dependency_too_high: {:.4} > 0.50",
            reward_dependency_ratio
        ));
    }
    if evidence.trades_truncated {
        warnings.push("public_trade_history_truncated_at_10000".into());
    }
    if evidence.activity.truncated {
        warnings.push("public_activity_history_truncated_at_10000".into());
    }
    if evidence.onchain.requested > 0 && evidence.onchain.loaded == 0 {
        warnings.push("onchain_counterparty_receipts_unavailable".into());
    }
    warnings.push(
        "historical_delay_copy_pnl_not_measured; current live_execution_fit is only a proxy".into(),
    );
    let scout_score = candidate
        .as_ref()
        .map(|candidate| candidate.score)
        .unwrap_or_default();
    let overall_score = round2(0.85 * scout_score + 0.15 * (100.0 - integrity_risk));
    let verdict = if candidate.is_none() {
        "INSUFFICIENT_DATA"
    } else if integrity_risk >= 50.0 {
        "REJECT_INTEGRITY"
    } else if reward_dependency_ratio > 0.50 {
        "RESEARCH_ONLY_REWARD_DEPENDENT"
    } else if candidate
        .as_ref()
        .is_some_and(|candidate| candidate.accepted)
    {
        "PAPER_CANDIDATE"
    } else {
        "REJECT"
    };
    WalletAuditConclusion {
        address: evidence.address.clone(),
        verdict: verdict.into(),
        worth_paper_copying: verdict == "PAPER_CANDIDATE",
        overall_score,
        integrity_risk_score: integrity_risk,
        scout: candidate,
        public_trades: evidence.trades.len(),
        distinct_trade_transactions: distinct_transactions,
        distinct_markets,
        observable_trade_volume_usdc: round2(trade_volume),
        rewards_and_rebates_usdc: round2(evidence.activity.rewards_usdc),
        reward_dependency_ratio: round4(reward_dependency_ratio),
        trade_history_truncated: evidence.trades_truncated,
        activity_history_truncated: evidence.activity.truncated,
        onchain_receipts_requested: evidence.onchain.requested,
        onchain_receipts_loaded: evidence.onchain.loaded,
        onchain_order_fills_matched: evidence.onchain.matched_order_fills,
        counterparty_fill_count_hhi: counterparty_hhi.map(round4),
        top_counterparties,
        hard_rejection_reasons,
        warnings,
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(condition: &str, side: &str, timestamp: i64, hash: &str) -> AuditTrade {
        AuditTrade {
            condition_id: condition.into(),
            asset: "1".into(),
            side: side.into(),
            size: 10.0,
            price: 0.5,
            timestamp,
            transaction_hash: hash.into(),
        }
    }

    #[test]
    fn normalizes_and_deduplicates_addresses() {
        let addresses = normalize_addresses(&[
            "0x1111111111111111111111111111111111111111".into(),
            "0x1111111111111111111111111111111111111111".into(),
            "0x2222222222222222222222222222222222222222".into(),
        ])
        .unwrap();
        assert_eq!(addresses.len(), 2);
    }

    #[test]
    fn synchronization_counts_each_left_trade_once() {
        let left = vec![
            trade("m1", "BUY", 100, "0xa"),
            trade("m1", "SELL", 200, "0xb"),
        ];
        let right = vec![
            trade("m1", "SELL", 105, "0xc"),
            trade("m1", "SELL", 106, "0xd"),
            trade("m1", "SELL", 205, "0xe"),
        ];
        assert_eq!(synchronized_trade_counts(&left, &right, 10), (1, 1));
    }

    #[test]
    fn parses_v2_order_filled_counterparty() {
        let target = "0x1111111111111111111111111111111111111111";
        let maker = "0x2222222222222222222222222222222222222222";
        let topic = |address: &str| format!("0x{:0>64}", &address[2..]);
        let receipt = json!({
            "logs": [{
                "address": CTF_EXCHANGE,
                "topics": [
                    format!("{:#x}", keccak256(b"OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")),
                    format!("0x{:064x}", 1),
                    topic(maker),
                    topic(target),
                ]
            }]
        });
        let (matched, counterparties) = counterparties_from_receipt(&receipt, target);
        assert_eq!(matched, 1);
        assert_eq!(counterparties, vec![maker]);
    }
}
