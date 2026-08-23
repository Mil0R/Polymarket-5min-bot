//! On-chain fill detection (review Opinion 1).
//!
//! Polymarket's Data-API indexes on-chain fills with a 3-15s (sometimes 30s)
//! delay. For fast-moving markets this means a copy signal can arrive after the
//! book has already moved past the price cap, causing missed trades.
//!
//! This module watches the CTF (ConditionalTokensFramework) ERC-1155
//! `TransferSingle`/`TransferBatch` events on Polygon via `eth_getLogs`, which
//! are emitted at settlement time for every fill regardless of which exchange
//! contract matched the order (the legacy CTFExchange `OrderFilled` event is no
//! longer emitted by Polymarket's current exchange — verified on-chain).
//!
//! Direction is inferred from the transfer path:
//!   - `to`   == whitelisted wallet -> wallet RECEIVED outcome shares -> BUY
//!   - `from` == whitelisted wallet -> wallet SENT outcome shares    -> SELL
//!
//! The ERC-1155 event does not carry the fill price; the caller estimates it
//! from the live book mid at signal time (documented approximation). The
//! execution engine re-prices from the live book independently, so this only
//! affects the entry price-cap sanity check.
//!
//! The Data-API `/trades` feed remains as a backfill for anything missed
//! between polls or across restarts.
//!
//! Env:
//!   COPY_ONCHAIN_FEED_ENABLED = true|false (default false)
//!   COPY_ONCHAIN_RPC_URL      = Polygon RPC (default publicnode)

use crate::copy_trading::feed::{outcome_from_label, DomainClassifier};
use crate::copy_trading::types::{DomainTag, OutcomeSide, SignalAction, SignalEvent};
use alloy::primitives::{address, keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{BlockNumberOrTag, Filter};
use alloy::sol;
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tracing::info;

sol! {
    #[derive(Debug, PartialEq)]
    event TransferSingle(address indexed operator, address indexed from, address indexed to, uint256 id, uint256 value);
    #[derive(Debug, PartialEq)]
    event TransferBatch(address indexed operator, address indexed from, address indexed to, uint256[] ids, uint256[] values);
}

/// ConditionalTokensFramework (CTF) on Polygon — the settlement layer that
/// emits ERC-1155 transfers for every outcome-token movement.
pub const CTF_POLYGON: Address = address!("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");

fn transfer_single_topic() -> B256 {
    keccak256(b"TransferSingle(address,address,address,uint256,uint256)")
}

fn transfer_batch_topic() -> B256 {
    keccak256(b"TransferBatch(address,address,address,uint256[],uint256[])")
}

/// Cached market resolution for a token: (condition_id, side, domain).
type TokenMarketInfo = (String, OutcomeSide, DomainTag);

pub struct OnChainTradeFeed {
    provider: Arc<dyn Provider>,
    last_block: AtomicU64,
    seen: Arc<DashMap<String, HashSet<String>>>,
    market_cache: Arc<DashMap<String, TokenMarketInfo>>,
    http: reqwest::Client,
}

use std::sync::atomic::{AtomicU64, Ordering};

impl OnChainTradeFeed {
    pub async fn new(rpc_url: &str) -> Result<Self> {
        let provider: Arc<dyn Provider> = Arc::new(
            ProviderBuilder::new()
                .connect_http(rpc_url.parse().context("invalid RPC URL")?),
        );
        let _block = provider
            .get_block_number()
            .await
            .context("RPC unreachable")?;
        info!(rpc = %rpc_url, "On-chain CTF transfer feed initialized");
        Ok(Self {
            provider,
            last_block: AtomicU64::new(0),
            seen: Arc::new(DashMap::new()),
            market_cache: Arc::new(DashMap::new()),
            http: reqwest::Client::builder()
                .user_agent("polypulse-copy-trader")
                .build()
                .context("failed to build http client")?,
        })
    }

    /// Seeds the poll cursor (used by verification probes to rewind to a known
    /// past block).
    pub fn set_last_block(&self, block: u64) {
        self.last_block.store(block, Ordering::SeqCst);
    }

    /// Polls CTF ERC-1155 transfers to/from the given wallets since the last
    /// poll. Returns newly observed signals (deduplicated by tx + log index).
    pub async fn poll_wallets(
        &self,
        wallets: &[Address],
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<SignalEvent>> {
        let latest = self.provider.get_block_number().await?;
        if wallets.is_empty() {
            self.last_block.fetch_max(latest, Ordering::SeqCst);
            return Ok(Vec::new());
        }
        let mut last = self.last_block.load(Ordering::SeqCst);
        #[cfg(debug_assertions)]
        eprintln!("[feed] last_block={last} latest={latest}");
        if last == 0 {
            // First poll: look back only a few blocks (CTF is the busiest
            // contract on Polygon; wide unfiltered lookbacks time out).
            last = latest.saturating_sub(10);
        }
        if latest <= last {
            return Ok(Vec::new());
        }

        let wallet_set: HashSet<Address> = wallets.iter().copied().collect();
        let mut signals = Vec::new();

        // Two pre-filtered queries per chunk keep result sets tiny even though
        // CTF emits enormous transfer streams:
        //   topic[2] (from) in wallets -> wallet SENT shares   (SELL fills)
        //   topic[3] (to)   in wallets -> wallet RECEIVED      (BUY fills)
        const MAX_BLOCKS_PER_QUERY: u64 = 30;
        let maker_topics: Vec<B256> = wallets
            .iter()
            .map(|m| {
                let mut bytes = [0u8; 32];
                bytes[12..].copy_from_slice(m.as_slice());
                B256::from(bytes)
            })
            .collect();

        let mut chunk_from = last + 1;
        loop {
            if chunk_from > latest {
                break;
            }
            let chunk_to = (chunk_from + MAX_BLOCKS_PER_QUERY - 1).min(latest);

            let base = Filter::new()
                .address(CTF_POLYGON)
                // OR-filter on the two transfer event signatures. NOTE: pass the
                // PRE-HASHED topics via event_signature — events() would hash
                // them AGAIN (double-keccak) and match nothing.
                .event_signature(vec![transfer_single_topic(), transfer_batch_topic()])
                .from_block(BlockNumberOrTag::Number(chunk_from))
                .to_block(BlockNumberOrTag::Number(chunk_to));
            let sell_filter = base.clone().topic2(maker_topics.clone());
            let buy_filter = base.clone().topic3(maker_topics.clone());

            #[cfg(debug_assertions)]
            {
                if let Ok(json) = serde_json::to_string(&buy_filter) {
                    eprintln!("[feed] buy_filter JSON: {json}");
                }
            }

            let sell_logs = self
                .provider
                .get_logs(&sell_filter)
                .await
                .context("CTF transfer (from) log query failed")?;
            let buy_logs = self
                .provider
                .get_logs(&buy_filter)
                .await
                .context("CTF transfer (to) log query failed")?;

            #[cfg(debug_assertions)]
            eprintln!(
                "[feed] chunk {chunk_from}..{chunk_to}: sell={} buy={}",
                sell_logs.len(),
                buy_logs.len()
            );

            let mut logs = sell_logs;
            logs.extend(buy_logs);

            #[cfg(debug_assertions)]
            eprintln!("[feed] chunk {chunk_from}..{chunk_to}: {} logs", logs.len());

            for log in &logs {
                let tx_hash = log.transaction_hash.unwrap_or_default();
                let log_idx = log
                    .log_index
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                let inner = log.clone().into_inner();

                #[cfg(debug_assertions)]
                eprintln!("[feed] raw log t0={}", inner.topics().first().map(|t| t.to_string()).unwrap_or_default());

                if let Ok(ev) = TransferSingle::decode_log(&inner) {
                    #[cfg(debug_assertions)]
                    eprintln!("[feed] TS from={:?} to={:?} id={}", ev.from, ev.to, ev.id);
                    if let Some(sig) = self
                        .transfer_to_signal(
                            &ev.from,
                            &ev.to,
                            &[ev.id],
                            &[ev.value],
                            &tx_hash,
                            &log_idx,
                            &wallet_set,
                            now,
                        )
                        .await
                    {
                        signals.push(sig);
                    }
                } else if let Ok(ev) = TransferBatch::decode_log(&inner) {
                    if let Some(sig) = self
                        .transfer_to_signal(
                            &ev.from,
                            &ev.to,
                            &ev.ids,
                            &ev.values,
                            &tx_hash,
                            &log_idx,
                            &wallet_set,
                            now,
                        )
                        .await
                    {
                        signals.push(sig);
                    }
                }
            }

            chunk_from = chunk_to + 1;
        }

        self.last_block.store(latest, Ordering::SeqCst);
        if !signals.is_empty() {
            info!(
                wallets = wallets.len(),
                fresh_signals = signals.len(),
                "On-chain feed produced new signals"
            );
        }
        Ok(signals)
    }

    #[allow(clippy::too_many_arguments)]
    async fn transfer_to_signal(
        &self,
        from: &Address,
        to: &Address,
        ids: &[U256],
        values: &[U256],
        tx_hash: &B256,
        log_idx: &str,
        wallet_set: &HashSet<Address>,
        now: chrono::DateTime<Utc>,
    ) -> Option<SignalEvent> {
        let action = match direction_from_path(from, to, wallet_set) {
            Some(a) => a,
            None => return None, // wallet-to-wallet transfer or neither side is a target
        };
        let wallet = if action == SignalAction::OpenBuy { *to } else { *from };

        // Aggregate all ids/values in this event for the target wallet.
        for (id, value) in ids.iter().zip(values.iter()) {
            if value.is_zero() {
                continue;
            }
            let shares = shares_from_raw(*value);
            if shares <= Decimal::ZERO {
                continue;
            }
            let token_id = id.to_string();
            let (market_id, outcome, domain) = match self.resolve_market(&token_id).await {
                Some(m) => m,
                None => continue,
            };
            let sig = SignalEvent {
                signal_id: format!("tx-{:x}-{}", tx_hash, log_idx),
                source_wallet: format!("0x{:x}", wallet),
                market_id,
                token_id,
                outcome,
                domain,
                action,
                // Price unknown from the transfer event; caller estimates from
                // the live book mid (0 marks "estimate me").
                target_vwap: Decimal::ZERO,
                target_shares: shares,
                target_notional_usdc: Decimal::ZERO,
                timestamp: now,
            };
            let key = sig.signal_id.clone();
            let mut seen_set = self.seen.entry(sig.source_wallet.clone()).or_default();
            if seen_set.len() > 5000 {
                seen_set.clear();
            }
            if seen_set.insert(key) {
                return Some(sig);
            }
        }
        None
    }

    /// Resolves a token id to (condition_id, outcome side, domain) via Gamma,
    /// cached per token (market structure is static per token).
    async fn resolve_market(&self, token_id: &str) -> Option<TokenMarketInfo> {
        if let Some(cached) = self.market_cache.get(token_id) {
            return Some(cached.clone());
        }
        // `closed=true` is required: gamma excludes resolved markets from
        // clob_token_ids lookups by default, and we must still resolve fills on
        // markets that settle while we are monitoring them.
        let url = format!(
            "https://gamma-api.polymarket.com/markets?clob_token_ids={}&limit=50&closed=true",
            token_id
        );
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let market = resp.as_array()?.first()?;
        let condition = market.get("conditionId")?.as_str()?.to_string();

        let parse_str_array = |v: &serde_json::Value| -> Vec<String> {
            match v {
                serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
                serde_json::Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                _ => Vec::new(),
            }
        };
        let outcomes = market
            .get("outcomes")
            .map(parse_str_array)
            .unwrap_or_default();
        let tokens = market
            .get("clobTokenIds")
            .map(parse_str_array)
            .unwrap_or_default();

        let idx = tokens.iter().position(|t| t == token_id)?;
        let label = outcomes.get(idx)?.clone();
        let outcome = outcome_from_label(&label);
        if !matches!(outcome, OutcomeSide::Yes | OutcomeSide::No) {
            return None;
        }
        let title = market
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("");
        let slug = market.get("slug").and_then(|s| s.as_str()).unwrap_or("");
        let domain = DomainClassifier::classify(&format!("{} {}", title, slug));

        let info = (condition, outcome, domain);
        self.market_cache.insert(token_id.to_string(), info.clone());
        Some(info)
    }
}

use rust_decimal_macros::dec;

/// Pure direction inference: does this ERC-1155 transfer represent a target
/// wallet BUY (receiving shares) or SELL (sending shares)?
fn direction_from_path(
    from: &Address,
    to: &Address,
    wallet_set: &HashSet<Address>,
) -> Option<SignalAction> {
    let is_buy = wallet_set.contains(to);
    let is_sell = wallet_set.contains(from);
    if is_buy && !is_sell {
        Some(SignalAction::OpenBuy)
    } else if is_sell && !is_buy {
        Some(SignalAction::CloseSell)
    } else {
        None
    }
}

/// Converts a raw ERC-1155 1e6-scaled amount to outcome shares.
fn shares_from_raw(raw: U256) -> Decimal {
    Decimal::from_str(&raw.to_string()).unwrap_or_default() / dec!(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn transfer_topics_match_erc1155_signatures() {
        assert_eq!(
            transfer_single_topic(),
            b256!("c3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62")
        );
        assert_eq!(
            transfer_batch_topic(),
            b256!("4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb")
        );
    }

    #[test]
    fn shares_scale_is_1e6() {
        assert_eq!(shares_from_raw(U256::from(100_000_000u64)), dec!(100));
    }

    #[test]
    fn direction_inference_uses_transfer_path() {
        let wallet: Address = "0x21f4c1a1be34a39a7bc20985df4b7e5a39fb691d"
            .parse()
            .unwrap();
        let other: Address = "0x408aee8c2c709e81d0ea89cbbbf42b7977ba2f5b"
            .parse()
            .unwrap();
        let set: HashSet<Address> = [wallet].into_iter().collect();

        // to=wallet -> BUY
        assert_eq!(
            direction_from_path(&other, &wallet, &set),
            Some(SignalAction::OpenBuy)
        );
        // from=wallet -> SELL
        assert_eq!(
            direction_from_path(&wallet, &other, &set),
            Some(SignalAction::CloseSell)
        );
        // wallet-to-wallet (ambiguous) -> None
        assert_eq!(direction_from_path(&wallet, &wallet, &set), None);
        // neither side is a target -> None
        assert_eq!(direction_from_path(&other, &other, &set), None);
    }
}
