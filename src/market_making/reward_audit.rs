use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use polymarket_client_sdk_v2::clob::types::request::UserRewardsEarningRequest;
use polymarket_client_sdk_v2::types::Address;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::str::FromStr;
use tokio::time::{interval, Duration, MissedTickBehavior};

use crate::{create_authenticated_clob_client, parse_signature_type};

use super::config::MarketMakingResearchConfig;

#[derive(Debug, Serialize)]
struct RewardMarketSnapshot {
    condition_id: String,
    market_slug: String,
    question: String,
    earning_percentage: Decimal,
    earnings_usdc: Decimal,
}

pub async fn record_reward_snapshot(
    config: &MarketMakingResearchConfig,
    date: NaiveDate,
) -> Result<()> {
    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
        .context("POLYMARKET_PRIVATE_KEY is required for read-only reward audit")?;
    let signature_type = parse_signature_type(
        &std::env::var("SIGNATURE_TYPE").unwrap_or_else(|_| "Poly1271".to_string()),
    );
    let funder = std::env::var("POLYMARKET_PROXY_ADDRESS")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Address::from_str(value.trim()))
        .transpose()
        .context("POLYMARKET_PROXY_ADDRESS has invalid format")?;
    let client = create_authenticated_clob_client(
        private_key.trim(),
        &config.clob_url,
        funder,
        signature_type,
    )
    .await?;
    let request = UserRewardsEarningRequest::builder().date(date).build();
    let mut cursor = None;
    let mut markets = Vec::new();
    loop {
        let page = client
            .user_earnings_and_markets_config(&request, cursor.clone())
            .await
            .context("failed to query authenticated maker reward earnings")?;
        for market in page.data {
            let earnings_usdc = market
                .earnings
                .iter()
                .map(|earning| earning.earnings * earning.asset_rate)
                .sum();
            markets.push(RewardMarketSnapshot {
                condition_id: market.condition_id.to_string(),
                market_slug: market.market_slug,
                question: market.question,
                earning_percentage: market.earning_percentage,
                earnings_usdc,
            });
        }
        if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
            break;
        }
        cursor = Some(page.next_cursor);
    }
    let total_earnings_usdc: Decimal = markets.iter().map(|market| market.earnings_usdc).sum();
    if let Some(parent) = config
        .reward_audit_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let event = json!({
        "action": "reward_snapshot",
        "version": 1,
        "timestamp": Utc::now(),
        "data": {
            "date": date,
            "market_count": markets.len(),
            "total_earnings_usdc": total_earnings_usdc,
            "markets": markets,
        },
    });
    let mut writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.reward_audit_file)
        .with_context(|| {
            format!(
                "failed to open reward audit file {}",
                config.reward_audit_file.display()
            )
        })?;
    serde_json::to_writer(&mut writer, &event)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    println!(
        "reward snapshot date={} markets={} earnings_usdc={} file={}",
        date,
        markets.len(),
        total_earnings_usdc.round_dp(6),
        config.reward_audit_file.display()
    );
    Ok(())
}

pub async fn watch_reward_snapshots(config: MarketMakingResearchConfig) -> Result<()> {
    let mut ticker = interval(Duration::from_secs(config.reward_poll_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(error) = record_reward_snapshot(&config, Utc::now().date_naive()).await {
                    tracing::error!(%error, "Maker reward audit failed");
                }
            }
            result = &mut shutdown => {
                result?;
                return Ok(());
            }
        }
    }
}
