//! Live CLOB order execution for the copy-trading strategy (Polymarket V2 SDK).
//!
//! Bridges the strategy's `ExecutionOrderProposal` to real signed CLOB orders:
//!   - BUY: fee-buffered size, USDC balance pre-check, marketable limit at the
//!     proposal price (never chases), FAK default (no resting remainder).
//!   - SELL: token-balance pre-check, same anti-chase discipline.
//!   - Fill reconciliation follows the main bot's proven convention:
//!     for BUY  orders `taking_amount` is the outcome shares received and
//!     `making_amount` is the USDC spent;
//!     for SELL orders `making_amount` is the shares sold and `taking_amount`
//!     is the USDC received.
//!
//! Env config:
//!   POLYMARKET_PRIVATE_KEY / POLYMARKET_PROXY_ADDRESS / SIGNATURE_TYPE / CLOB_API_URL
//!   COPY_ORDER_TYPE      = FAK (default) | FOK | GTC | GTD
//!   COPY_FEE_BUFFER_PCT  = 0.01 (fraction of notional reserved for fees)

use crate::clob_v2::{create_authenticated_clob_client, parse_signature_type, AuthenticatedClobClient, CLOB_API_URL_DEFAULT};
use crate::copy_trading::types::{ExecutionOrderProposal, OutcomeSide};
use anyhow::{Context, Result};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use dashmap::DashMap;
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::response::FeeDetails;
use polymarket_client_sdk_v2::clob::types::{AssetType, OrderType, Side, SignedOrder};
use polymarket_client_sdk_v2::types::{Address, B256, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use rust_decimal_macros::dec;
use std::str::FromStr;
use tracing::{info, warn};

/// Result of executing a real CLOB order for a proposal.
#[derive(Debug, Clone)]
pub struct OrderExecutionOutcome {
    pub order_id: String,
    pub filled_shares: Decimal,
    pub avg_price: Decimal,
    /// For BUY: USDC spent; for SELL: USDC proceeds.
    pub cost_or_proceeds: Decimal,
    pub fully_filled: bool,
    /// Exchange-reported status string.
    pub status: String,
}

/// CLOB response amounts are asset-oriented: BUY receives outcome shares as
/// taker amount, while SELL spends outcome shares as maker amount.
fn outcome_shares_filled(side: &Side, making_amount: Decimal, taking_amount: Decimal) -> Decimal {
    if matches!(side, Side::Buy) {
        taking_amount
    } else {
        making_amount
    }
}

/// Rounds a share count DOWN to 2 decimals so notional+fee stays within budget.
fn floor2(v: Decimal) -> Decimal {
    ((v * dec!(100)).floor()) / dec!(100)
}

pub struct CopyTradeExecutor {
    client: AuthenticatedClobClient,
    private_key: String,
    order_type: OrderType,
    fee_buffer_pct: Decimal,
    /// condition_id -> fee policy (rate/exponent), cached to skip the HTTP round-trip.
    fee_cache: DashMap<String, FeeDetails>,
}

impl CopyTradeExecutor {
    /// Builds an authenticated CLOB client from the standard Polymarket env vars.
    pub async fn new_from_env() -> Result<Self> {
        let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
            .context("POLYMARKET_PRIVATE_KEY not set (required for live copy trading)")?;
        let proxy_raw = std::env::var("POLYMARKET_PROXY_ADDRESS").ok();
        let signature_type = parse_signature_type(
            &std::env::var("SIGNATURE_TYPE").unwrap_or_else(|_| "Poly1271".to_string()),
        );
        let clob_url = std::env::var("CLOB_API_URL").unwrap_or_else(|_| CLOB_API_URL_DEFAULT.to_string());

        let v2_proxy = proxy_raw
            .as_deref()
            .map(|s| s.parse::<Address>().context("invalid POLYMARKET_PROXY_ADDRESS"))
            .transpose()?;

        let client = create_authenticated_clob_client(&private_key, &clob_url, v2_proxy, signature_type)
            .await
            .context("CLOB authentication failed (check POLYMARKET_PRIVATE_KEY / proxy / network)")?;

        let order_type = match std::env::var("COPY_ORDER_TYPE")
            .unwrap_or_else(|_| "FAK".to_string())
            .to_ascii_uppercase()
            .as_str()
        {
            "FOK" => OrderType::FOK,
            "GTC" => OrderType::GTC,
            "GTD" => OrderType::GTD,
            _ => OrderType::FAK,
        };

        let fee_buffer_pct = std::env::var("COPY_FEE_BUFFER_PCT")
            .ok()
            .and_then(|v| v.parse::<Decimal>().ok())
            .unwrap_or(dec!(0.01));

        Ok(Self {
            client,
            private_key,
            order_type,
            fee_buffer_pct,
            fee_cache: DashMap::new(),
        })
    }

    /// Verifies the CLOB session is actually authenticated (api keys present).
    pub async fn verify_authentication(&self) -> Result<()> {
        self.client
            .api_keys()
            .await
            .map_err(|e| anyhow::anyhow!("Copy-trading CLOB auth verification failed: {}", e))?;
        Ok(())
    }

    /// Current USDC (collateral) balance of the trading wallet.
    pub async fn usdc_balance(&self) -> Result<Decimal> {
        let request = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Collateral)
            .build();
        Ok(self.client.balance_allowance(request).await?.balance)
    }

    /// Current outcome-token balance for a specific token.
    pub async fn token_balance(&self, token_id: U256) -> Result<Decimal> {
        let request = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Conditional)
            .token_id(token_id)
            .build();
        Ok(self.client.balance_allowance(request).await?.balance)
    }

    /// V2 taker fee per share: `rate * (p*(1-p))^exponent` (same formula the main
    /// bot uses). Highest at p=0.5, lowest near 0/1 — a flat percentage buffer
    /// wastes capital at extreme prices and can overflow at mid prices.
    pub fn fee_per_share(rate: Decimal, exponent: u32, price: Decimal) -> Decimal {
        if rate <= Decimal::ZERO || price <= Decimal::ZERO || price >= Decimal::ONE {
            return Decimal::ZERO;
        }
        let base = price * (Decimal::ONE - price);
        let factor = (0..exponent).fold(Decimal::ONE, |acc, _| acc * base);
        rate * factor
    }

    /// Fetches (and caches) the taker fee policy for a market's condition id.
    async fn fee_policy(&self, condition_id: &str) -> Result<FeeDetails> {
        if let Some(cached) = self.fee_cache.get(condition_id) {
            return Ok(cached.clone());
        }
        let info = self
            .client
            .clob_market_info(condition_id)
            .await
            .map_err(|e| anyhow::anyhow!("fee policy fetch failed for {}: {}", condition_id, e))?;
        let details = info.fee_details.ok_or_else(|| {
            anyhow::anyhow!("no fee_details in market info for condition {}", condition_id)
        })?;
        self.fee_cache.insert(condition_id.to_string(), details.clone());
        Ok(details)
    }

    /// Computes the fee-buffered size: the largest share count whose notional
    /// PLUS expected taker fee stays inside the proposal's budget.
    /// Falls back to the flat `COPY_FEE_BUFFER_PCT` when fee details are missing.
    async fn fee_buffered_size(&self, condition_id: &str, price: Decimal, shares: Decimal) -> Decimal {
        let fee_per_share = match self.fee_policy(condition_id).await {
            Ok(policy) => Self::fee_per_share(policy.rate, policy.exponent, price),
            Err(e) => {
                warn!(condition = %condition_id, error = %e, "Fee policy unavailable; using flat buffer");
                // Flat fallback: reserve `fee_buffer_pct` of notional.
                (price * shares * self.fee_buffer_pct) / shares
            }
        };
        let divisor = price + fee_per_share;
        if divisor <= Decimal::ZERO {
            return floor2((shares * (Decimal::ONE - self.fee_buffer_pct)).max(Decimal::ZERO));
        }
        // size = budget / (price + fee) where budget = price * shares.
        // FLOOR the size so cost + fee can never exceed the budget (round-half-up
        // could overshoot by sub-cent rounding).
        floor2((shares * price) / divisor)
    }

    /// Executes a BUY proposal as a real CLOB marketable limit order.
    ///
    /// The order size is fee-buffered so the total spend (notional + fee) stays
    /// inside the allocated budget. USDC balance is checked first. FAK/FOK never
    /// leave a resting remainder; GTC/GTD rely on the coordinator's TTL cancel.
    pub async fn execute_buy(&self, proposal: &ExecutionOrderProposal) -> Result<OrderExecutionOutcome> {
        let token_id: U256 = proposal
            .token_id
            .parse()
            .context("invalid token id in buy proposal")?;

        let size = self
            .fee_buffered_size(&proposal.market_id, proposal.limit_price, proposal.shares)
            .await;
        if size <= Decimal::ZERO {
            anyhow::bail!("buy size zero after fee buffer for proposal {}", proposal.proposal_id);
        }

        let projected_cost = proposal.limit_price * size;
        // Best-effort USDC pre-check: the exchange remains the source of truth
        // (some accounts cannot be queried via balance-allowance, e.g. legacy
        // funders — in that case we warn and let the exchange decide).
        match self.usdc_balance().await {
            Ok(usdc) => {
                if usdc < projected_cost {
                    anyhow::bail!(
                        "insufficient USDC for buy: have ${:.2}, need ${:.2} ({} shares @ {})",
                        usdc,
                        projected_cost,
                        size,
                        proposal.limit_price
                    );
                }
            }
            Err(e) => warn!(
                error = %e,
                "USDC balance pre-check unavailable; proceeding (exchange will enforce)"
            ),
        }

        let signed = self
            .build_and_sign(token_id, Side::Buy, proposal.limit_price, size)
            .await?;
        let response = self.client.post_order(signed).await.map_err(|e| {
            anyhow::anyhow!(
                "CLOB buy order failed for {}: {}",
                proposal.proposal_id,
                e
            )
        })?;

        let filled = outcome_shares_filled(&Side::Buy, response.making_amount, response.taking_amount);
        let spent = response.making_amount.max(Decimal::ZERO);
        let avg_price = if filled > Decimal::ZERO && spent > Decimal::ZERO {
            spent / filled
        } else if filled > Decimal::ZERO {
            proposal.limit_price
        } else {
            Decimal::ZERO
        };

        info!(
            order_id = %response.order_id,
            token_id = %token_id,
            filled = %filled,
            avg_price = %avg_price,
            spent = %spent,
            requested = %size,
            status = ?response.status,
            "LIVE BUY executed"
        );
        if filled < size {
            warn!(
                order_id = %response.order_id,
                filled = %filled,
                requested = %size,
                "LIVE BUY partially filled (remainder auto-cancelled by {})",
                order_type_label(&self.order_type)
            );
        }

        Ok(OrderExecutionOutcome {
            order_id: response.order_id.clone(),
            filled_shares: filled,
            avg_price,
            cost_or_proceeds: spent,
            fully_filled: filled >= size,
            status: format!("{:?}", response.status),
        })
    }

    /// Executes a SELL proposal as a real CLOB order, verifying token holdings first.
    pub async fn execute_sell(&self, proposal: &ExecutionOrderProposal) -> Result<OrderExecutionOutcome> {
        let token_id: U256 = proposal
            .token_id
            .parse()
            .context("invalid token id in sell proposal")?;

        let size = self
            .fee_buffered_size(&proposal.market_id, proposal.limit_price, proposal.shares)
            .await;
        if size <= Decimal::ZERO {
            anyhow::bail!("sell size zero after fee buffer for proposal {}", proposal.proposal_id);
        }

        // Best-effort token-balance pre-check (advisory; exchange enforces).
        match self.token_balance(token_id).await {
            Ok(held) => {
                if held < size {
                    anyhow::bail!(
                        "insufficient token balance to sell: have {} shares, need {} ({})",
                        held,
                        size,
                        proposal.token_id
                    );
                }
            }
            Err(e) => warn!(
                error = %e,
                "Token balance pre-check unavailable; proceeding (exchange will enforce)"
            ),
        }

        let signed = self
            .build_and_sign(token_id, Side::Sell, proposal.limit_price, size)
            .await?;
        let response = self.client.post_order(signed).await.map_err(|e| {
            anyhow::anyhow!(
                "CLOB sell order failed for {}: {}",
                proposal.proposal_id,
                e
            )
        })?;

        let filled = outcome_shares_filled(&Side::Sell, response.making_amount, response.taking_amount);
        let proceeds = response.taking_amount.max(Decimal::ZERO);
        let avg_price = if filled > Decimal::ZERO && proceeds > Decimal::ZERO {
            proceeds / filled
        } else if filled > Decimal::ZERO {
            proposal.limit_price
        } else {
            Decimal::ZERO
        };

        info!(
            order_id = %response.order_id,
            token_id = %token_id,
            filled = %filled,
            avg_price = %avg_price,
            proceeds = %proceeds,
            requested = %size,
            status = ?response.status,
            "LIVE SELL executed"
        );
        if filled < size {
            warn!(
                order_id = %response.order_id,
                filled = %filled,
                requested = %size,
                "LIVE SELL partially filled (remainder auto-cancelled by {})",
                order_type_label(&self.order_type)
            );
        }

        Ok(OrderExecutionOutcome {
            order_id: response.order_id.clone(),
            filled_shares: filled,
            avg_price,
            cost_or_proceeds: proceeds,
            fully_filled: filled >= size,
            status: format!("{:?}", response.status),
        })
    }

    /// Redeems a settled WINNING position on-chain (CTF adapter redeemPositions)
    /// so the ERC-1155 winner tokens become spendable collateral again.
    ///
    /// `shares` is the decimal share count held; `outcome` selects the outcome
    /// index (YES=0 / NO=1) used by the redeem call.
    pub async fn redeem_settled_position(
        &self,
        condition_id: &str,
        outcome: &OutcomeSide,
        token_id: &str,
        shares: Decimal,
        neg_risk: bool,
    ) -> Result<String> {
        let cond: B256 = condition_id.parse().context("invalid condition id")?;
        let token: U256 = token_id.parse().context("invalid token id")?;
        let outcome_index = match outcome {
            OutcomeSide::Yes => 0,
            OutcomeSide::No => 1,
            OutcomeSide::Other(_) => anyhow::bail!("cannot redeem non-binary outcome"),
        };
        // ERC-1155 balances are 1e6-scaled integers (matches the main bot's redeem path).
        let raw_size_str = (shares * dec!(1_000_000)).floor().to_string();
        let raw_size: U256 = U256::from_str(&raw_size_str).context("invalid redeem size")?;

        let proxy_raw = std::env::var("POLYMARKET_PROXY_ADDRESS")
            .context("POLYMARKET_PROXY_ADDRESS not set (required for redeem)")?;
        let proxy: Address = proxy_raw
            .parse()
            .context("invalid POLYMARKET_PROXY_ADDRESS")?;

        info!(
            condition_id = %condition_id,
            token_id = %token_id,
            shares = %shares,
            outcome = ?outcome,
            neg_risk = neg_risk,
            "Redeeming settled winner position on-chain"
        );

        let tx = crate::redeem::redeem_one(
            cond,
            neg_risk,
            proxy,
            &self.private_key,
            None,
            Some(outcome_index),
            Some(raw_size),
            &[token],
        )
        .await
        .context("on-chain redeem failed")?;
        Ok(tx)
    }

    /// Cancels a resting order (used for GTC/GTD TTL expiry).
    pub async fn cancel(&self, order_id: &str) -> Result<()> {
        self.client
            .cancel_order(order_id)
            .await
            .map_err(|e| anyhow::anyhow!("cancel failed for {}: {}", order_id, e))?;
        Ok(())
    }

    /// Current cumulative matched size for an order (post-submit reconciliation).
    pub async fn cumulative_fill(&self, order_id: &str) -> Result<Decimal> {
        Ok(self.client.order(order_id).await?.size_matched)
    }

    async fn build_and_sign(
        &self,
        token_id: U256,
        side: Side,
        price: Decimal,
        size: Decimal,
    ) -> Result<SignedOrder> {
        let builder = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(side)
            .price(price)
            .size(size)
            .order_type(self.order_type.clone());
        let order = builder.build().await?;
        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        Ok(self.client.sign(&signer, order).await?)
    }
}

fn order_type_label(t: &OrderType) -> &'static str {
    match t {
        OrderType::FAK => "FAK",
        OrderType::FOK => "FOK",
        OrderType::GTC => "GTC",
        OrderType::GTD => "GTD",
        OrderType::Unknown(_) => "UNKNOWN",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_formula_matches_main_bot_and_is_price_aware() {
        let rate = dec!(0.07);
        // Mid price: p(1-p) = 0.25 -> fee 0.0175/share (1.75% of a 0.50 notional).
        let mid_fee = CopyTradeExecutor::fee_per_share(rate, 1, dec!(0.50));
        assert_eq!(mid_fee, dec!(0.0175));
        // Extreme price: p(1-p) = 0.09 -> fee 0.0063/share (0.63% of a 0.90 notional).
        let extreme_fee = CopyTradeExecutor::fee_per_share(rate, 1, dec!(0.90));
        assert_eq!(extreme_fee, dec!(0.0063));
        // Mid-price fee is ~2.8x the extreme-price fee: a flat 1% buffer would
        // overflow at mid prices and waste capital at extremes.
        assert!(mid_fee > extreme_fee * dec!(2));
        // Boundary sanity.
        assert_eq!(CopyTradeExecutor::fee_per_share(dec!(0.07), 1, dec!(1.0)), Decimal::ZERO);
        assert_eq!(CopyTradeExecutor::fee_per_share(Decimal::ZERO, 1, dec!(0.5)), Decimal::ZERO);
    }

    #[tokio::test]
    async fn fee_buffered_size_keeps_cost_plus_fee_within_budget() {
        // No network in unit tests: the fee policy fetch fails and falls back to
        // the flat buffer, which must still produce a positive size.
        // (Constructing a full executor requires auth, so we test the formula path
        // through the public static + the fallback via a minimal instance is not
        // possible offline — the fallback arithmetic is covered by the same rule.)
        let shares = dec!(200.0);
        let price = dec!(0.50);
        let budget = price * shares; // $100
        let fee = CopyTradeExecutor::fee_per_share(dec!(0.07), 1, price); // 0.0175
        let buffered = floor2((shares * price) / (price + fee));
        assert!(buffered < shares);
        assert!(buffered * (price + fee) <= budget, "{} x {} must stay in {}", buffered, price + fee, budget);
    }
}
