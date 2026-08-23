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
use crate::copy_trading::types::ExecutionOrderProposal;
use anyhow::{Context, Result};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer;
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::{AssetType, OrderType, Side, SignedOrder};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
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

pub struct CopyTradeExecutor {
    client: AuthenticatedClobClient,
    private_key: String,
    order_type: OrderType,
    fee_buffer_pct: Decimal,
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

        let size = (proposal.shares * (Decimal::ONE - self.fee_buffer_pct))
            .max(Decimal::ZERO)
            .round_dp(2);
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

        let size = (proposal.shares * (Decimal::ONE - self.fee_buffer_pct))
            .max(Decimal::ZERO)
            .round_dp(2);
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
