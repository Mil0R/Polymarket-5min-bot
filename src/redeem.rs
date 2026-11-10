//! CTF Redeem: redeem winner tokens for settled markets to pUSD (V2) or USDC.e (legacy).
//!
//! Supports **Gnosis Safe** (execTransaction) and **Magic/Email EIP-1167** (Polymarket Relayer).
//! V2 默认经 CollateralAdapter 赎回为 pUSD；设 `REDEEM_OUTPUT_TOKEN=USDC.e` 可走 legacy 路径。
//! pUSD 首次赎回需对 CTF 执行 `setApprovalForAll(adapter, true)`，本模块会自动处理。

use std::env;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use anyhow::Result;
use polymarket_client_sdk::{contract_config, POLYGON};
use std::str::FromStr as _;
use tracing::{info, warn};

use alloy::sol;
sol! {
    #[sol(rpc)]
    interface IWinnerPayout {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function payoutNumerators(bytes32 conditionId, uint256 index) external view returns (uint256);
        function getCollectionId(bytes32 parentCollectionId, bytes32 conditionId, uint256 indexSet) external view returns (bytes32);
        function getPositionId(address collateralToken, bytes32 collectionId) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IERC1155 {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

use crate::adapter_auth::{encode_set_approval_for_all, ensure_adapter_approved, wait_relayer_tx};
use crate::deposit_wallet_relay::{
    relayer_execute_deposit_wallet_calldata, use_deposit_wallet_relayer,
};
use crate::proxy_relay::{
    derive_proxy_wallet, relayer_execute_proxy_calldata, IGnosisSafe, RelayerCredentials,
    CTF_COLLATERAL_ADAPTER, CTF_POLYGON, NEG_RISK_ADAPTER, NEG_RISK_COLLATERAL_ADAPTER,
    PROXY_FACTORY, PROXY_REDEEM_LEGACY_GAS, PROXY_REDEEM_PUSD_GAS, PUSD_POLYGON,
    RELAYER_URL_DEFAULT, RPC_URL_DEFAULT, USDC_POLYGON,
};

const PARENT_COLLECTION_ID: B256 = B256::ZERO;

fn encode_redeem_calldata_4arg(collateral_token: Address, condition_id: B256) -> Vec<u8> {
    let sel = &keccak256(b"redeemPositions(address,bytes32,bytes32,uint256[])")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(collateral_token.as_slice());
    out.extend_from_slice(PARENT_COLLECTION_ID.as_slice());
    out.extend_from_slice(condition_id.as_slice());
    out.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(1u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out
}

fn encode_redeem_calldata_neg_risk_legacy(condition_id: B256, amounts: [U256; 2]) -> Vec<u8> {
    let sel = &keccak256(b"redeemPositions(bytes32,uint256[])")[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(condition_id.as_slice());
    out.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>());
    out.extend_from_slice(&U256::from(2u64).to_be_bytes::<32>());
    out.extend_from_slice(&amounts[0].to_be_bytes::<32>());
    out.extend_from_slice(&amounts[1].to_be_bytes::<32>());
    out
}

fn redeem_to_pusd() -> bool {
    match env::var("REDEEM_OUTPUT_TOKEN") {
        Ok(v) => {
            let s = v.trim().to_lowercase();
            s != "usdc.e" && s != "usdc"
        }
        Err(_) => true,
    }
}

fn pusd_adapter(neg_risk: bool) -> Address {
    if neg_risk {
        NEG_RISK_COLLATERAL_ADAPTER
    } else {
        CTF_COLLATERAL_ADAPTER
    }
}

fn resolve_redeem_call(
    neg_risk: bool,
    condition_id: B256,
    outcome_index: Option<i32>,
    size_raw: Option<U256>,
) -> Result<(Address, Vec<u8>)> {
    if redeem_to_pusd() {
        let target = pusd_adapter(neg_risk);
        let calldata = encode_redeem_calldata_4arg(PUSD_POLYGON, condition_id);
        return Ok((target, calldata));
    }

    if neg_risk {
        let idx = outcome_index.ok_or_else(|| {
            anyhow::anyhow!(
                "NegRisk legacy USDC.e 赎回需要 outcome_index（或设 REDEEM_OUTPUT_TOKEN=pUSD）"
            )
        })?;
        let size = size_raw.ok_or_else(|| {
            anyhow::anyhow!(
                "NegRisk legacy USDC.e 赎回需要持仓 size（或设 REDEEM_OUTPUT_TOKEN=pUSD）"
            )
        })?;
        let mut amounts = [U256::ZERO, U256::ZERO];
        if idx == 0 || idx == 1 {
            amounts[idx as usize] = size;
        } else {
            anyhow::bail!("无效的 outcome_index: {}", idx);
        }
        let calldata = encode_redeem_calldata_neg_risk_legacy(condition_id, amounts);
        return Ok((NEG_RISK_ADAPTER, calldata));
    }

    let config = contract_config(POLYGON, false)
        .ok_or_else(|| anyhow::anyhow!("Unsupported chain_id: {}", POLYGON))?;
    let calldata = encode_redeem_calldata_4arg(USDC_POLYGON, condition_id);
    Ok((config.conditional_tokens, calldata))
}

async fn outcome_balance<P: Provider>(provider: &P, proxy: Address, asset: U256) -> Result<U256> {
    let ctf = IERC1155::new(CTF_POLYGON, provider);
    ctf.balanceOf(proxy, asset)
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("balanceOf failed: {}", e))
}

async fn verify_redeemed<P: Provider>(
    provider: &P,
    proxy: Address,
    assets: &[U256],
    before: &[U256],
) -> Result<()> {
    for (asset, prev) in assets.iter().zip(before.iter()) {
        let after = outcome_balance(provider, proxy, *asset).await?;
        if after >= *prev {
            anyhow::bail!(
                "赎回后 outcome token {} 余额未减少 (before={} after={})，Relayer 可能 gas 不足或链上 redeem 失败",
                asset,
                prev,
                after
            );
        }
    }
    Ok(())
}

fn redeem_gas_limit() -> u64 {
    if redeem_to_pusd() {
        PROXY_REDEEM_PUSD_GAS
    } else {
        PROXY_REDEEM_LEGACY_GAS
    }
}

fn use_relayer_by_config(code_len: usize) -> bool {
    if use_deposit_wallet_relayer() {
        return true;
    }
    let s = match env::var("SIGNATURE_TYPE") {
        Ok(v) => v.trim().to_lowercase(),
        Err(_) => return code_len < 150,
    };
    if s == "proxy" {
        return true;
    }
    if s == "gnosissafe" || s == "safe" {
        return false;
    }
    code_len < 150
}

#[allow(clippy::too_many_arguments)] // Wallet-kind compatibility adapter.
async fn relayer_redeem_calldata(
    redeem_data: &[u8],
    redeem_to: Address,
    wallet: Address,
    signer: &impl alloy::signers::Signer,
    builder_key: &str,
    builder_secret: &str,
    builder_passphrase: &str,
    relayer_url: &str,
    gas_limit: Option<u64>,
) -> Result<String> {
    if use_deposit_wallet_relayer() {
        relayer_execute_deposit_wallet_calldata(
            redeem_data,
            redeem_to,
            wallet,
            signer,
            RelayerCredentials {
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
            },
            "Redeem positions",
        )
        .await
    } else {
        relayer_execute_proxy_calldata(
            redeem_data,
            redeem_to,
            wallet,
            signer,
            RelayerCredentials {
                builder_key,
                builder_secret,
                builder_passphrase,
                relayer_url,
            },
            "Redeem positions",
            gas_limit,
        )
        .await
    }
}

async fn safe_exec_call<P: Provider>(
    safe: &IGnosisSafe::IGnosisSafeInstance<P>,
    signer: &impl alloy::signers::Signer,
    to: Address,
    calldata: Vec<u8>,
) -> Result<B256> {
    let nonce: U256 = safe
        .nonce()
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read Safe nonce: {}", e))?;
    let tx_hash_data = safe
        .encodeTransactionData(
            to,
            U256::ZERO,
            calldata.clone().into(),
            0u8,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            Address::ZERO,
            Address::ZERO,
            nonce,
        )
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Safe.encodeTransactionData failed: {}", e))?
        .0;
    let tx_hash = keccak256(tx_hash_data.as_ref());
    let sig = signer
        .sign_hash(&tx_hash)
        .await
        .map_err(|e| anyhow::anyhow!("Signing failed: {}", e))?;
    let mut sig_bytes = sig.as_bytes().to_vec();
    if sig_bytes.len() == 65 && (sig_bytes[64] == 0 || sig_bytes[64] == 1) {
        sig_bytes[64] += 27;
    }
    let pending = safe
        .execTransaction(
            to,
            U256::ZERO,
            calldata.into(),
            0u8,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            Address::ZERO,
            Address::ZERO,
            sig_bytes.into(),
        )
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Safe.execTransaction failed: {}", e))?;
    let tx_hash_out = *pending.tx_hash();
    let receipt = pending
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("Failed waiting for receipt: {}", e))?;
    if !receipt.status() {
        anyhow::bail!("Safe tx reverted: {:#x}", tx_hash_out);
    }
    Ok(tx_hash_out)
}

async fn build_redeem_calls<P: Provider>(
    provider: &P,
    proxy: Address,
    neg_risk: bool,
    redeem_target: Address,
    redeem_calldata: Vec<u8>,
) -> Result<Vec<(Address, Vec<u8>)>> {
    let _ = (provider, proxy, neg_risk);
    Ok(vec![(redeem_target, redeem_calldata)])
}

/// Redeem winner tokens for `condition_id` on `proxy`.
/// `verify_assets`: 赎回前后校验这些 outcome token 余额是否减少（可为空跳过）。
#[allow(clippy::too_many_arguments)] // Public compatibility boundary; use a request type in the next API revision.
pub async fn redeem_one(
    condition_id: B256,
    neg_risk: bool,
    proxy: Address,
    private_key: &str,
    rpc_url: Option<&str>,
    outcome_index: Option<i32>,
    size_raw: Option<U256>,
    verify_assets: &[U256],
) -> Result<String> {
    let rpc = rpc_url.unwrap_or(RPC_URL_DEFAULT);
    let chain = POLYGON;
    let signer = LocalSigner::from_str(private_key)?.with_chain_id(Some(chain));
    let wallet = signer.address();

    let (redeem_target, redeem_calldata) =
        resolve_redeem_call(neg_risk, condition_id, outcome_index, size_raw)?;

    let output = if redeem_to_pusd() { "pUSD" } else { "USDC.e" };
    info!(
        "Redeem {:?} | neg_risk={} | target={:?} | output={}",
        condition_id, neg_risk, redeem_target, output
    );

    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(rpc)
        .await?;
    let mut balances_before = Vec::new();
    for asset in verify_assets {
        balances_before.push(outcome_balance(&provider, proxy, *asset).await?);
    }
    let configured_gas_limit = redeem_gas_limit();
    let gas_limit = Some(configured_gas_limit);
    info!("Relayer redeem gasLimit={configured_gas_limit}");
    let calls =
        build_redeem_calls(&provider, proxy, neg_risk, redeem_target, redeem_calldata).await?;
    let code = provider.get_code_at(proxy).await.unwrap_or_default();
    let use_relayer = use_relayer_by_config(code.len());

    if use_relayer {
        if !use_deposit_wallet_relayer() {
            let derived = derive_proxy_wallet(wallet, PROXY_FACTORY);
            let try_anyway = env::var("MERGE_TRY_ANYWAY")
                .map(|s| s.trim() == "1" || s.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if derived != proxy && !try_anyway {
                anyhow::bail!(
                    "POLYMARKET_PROXY_ADDRESS ({:?}) does not match ProxyFactory derive ({:?}). Set MERGE_TRY_ANYWAY=1 to force.",
                    proxy, derived
                );
            }
            if derived != proxy {
                warn!("MERGE_TRY_ANYWAY=1: derive != proxy, still sending Relayer request.");
            }
        }
        let builder_key = env::var("POLY_BUILDER_API_KEY").ok();
        let builder_secret = env::var("POLY_BUILDER_SECRET").ok();
        let builder_passphrase = env::var("POLY_BUILDER_PASSPHRASE").ok();
        let relayer_url =
            env::var("RELAYER_URL").unwrap_or_else(|_| RELAYER_URL_DEFAULT.to_string());
        match (builder_key.as_deref(), builder_secret.as_deref(), builder_passphrase.as_deref()) {
            (Some(k), Some(s), Some(p)) => {
                if redeem_to_pusd() {
                    ensure_adapter_approved(
                        &provider,
                        proxy,
                        pusd_adapter(neg_risk),
                        &signer,
                        RelayerCredentials {
                            builder_key: k,
                            builder_secret: s,
                            builder_passphrase: p,
                            relayer_url: &relayer_url,
                        },
                    )
                    .await?;
                }
                let (redeem_to, redeem_data) = calls
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("missing redeem call"))?;
                let out = relayer_redeem_calldata(
                    &redeem_data,
                    redeem_to,
                    proxy,
                    &signer,
                    k,
                    s,
                    p,
                    &relayer_url,
                    gas_limit,
                )
                .await?;
                wait_relayer_tx(&provider, &out).await?;
                if !verify_assets.is_empty() {
                    verify_redeemed(&provider, proxy, verify_assets, &balances_before).await?;
                }
                info!("✅ Relayer redeem confirmed: {}", out);
                return Ok(out);
            }
            _ => anyhow::bail!(
                "Magic/Email requires POLY_BUILDER_API_KEY, POLY_BUILDER_SECRET, POLY_BUILDER_PASSPHRASE.",
            ),
        }
    }

    let safe = IGnosisSafe::new(proxy, provider.clone());
    if redeem_to_pusd() {
        let adapter = pusd_adapter(neg_risk);
        if !crate::adapter_auth::is_adapter_approved(&provider, CTF_POLYGON, proxy, adapter).await?
        {
            let approve_calldata = encode_set_approval_for_all(adapter, true);
            let tx = safe_exec_call(&safe, &signer, CTF_POLYGON, approve_calldata).await?;
            info!("✅ Safe setApprovalForAll tx: {:#x}", tx);
        }
    }
    let mut last_tx = B256::ZERO;
    for (to, calldata) in calls {
        last_tx = safe_exec_call(&safe, &signer, to, calldata).await?;
        info!("✅ Safe redeem tx: {:#x}", last_tx);
    }
    if !verify_assets.is_empty() {
        verify_redeemed(&provider, proxy, verify_assets, &balances_before).await?;
    }
    Ok(format!("{:#x}", last_tx))
}

/// Authoritative CTF payout. A zero denominator is unresolved, never a timeout loss.
/// Returns payouts in contract outcome order, including fractional/void outcomes.
pub async fn condition_token_payout(
    condition_id: B256,
    token: U256,
) -> Result<Option<(u8, rust_decimal::Decimal)>> {
    let provider = ProviderBuilder::new().connect(RPC_URL_DEFAULT).await?;
    let ctf = IWinnerPayout::new(CTF_POLYGON, &provider);
    let denominator = ctf.payoutDenominator(condition_id).call().await?;
    if denominator.is_zero() {
        return Ok(None);
    }
    let (a, b, c0, c1) = tokio::try_join!(
        async { ctf.payoutNumerators(condition_id, U256::ZERO).call().await },
        async {
            ctf.payoutNumerators(condition_id, U256::from(1))
                .call()
                .await
        },
        async {
            ctf.getCollectionId(B256::ZERO, condition_id, U256::from(1))
                .call()
                .await
        },
        async {
            ctf.getCollectionId(B256::ZERO, condition_id, U256::from(2))
                .call()
                .await
        },
    )?;
    let (legacy0, pusd0, legacy1, pusd1) = tokio::try_join!(
        async { ctf.getPositionId(USDC_POLYGON, c0).call().await },
        async { ctf.getPositionId(PUSD_POLYGON, c0).call().await },
        async { ctf.getPositionId(USDC_POLYGON, c1).call().await },
        async { ctf.getPositionId(PUSD_POLYGON, c1).call().await },
    )?;
    let zero = token == legacy0 || token == pusd0;
    let one = token == legacy1 || token == pusd1;
    anyhow::ensure!(
        zero != one,
        "token does not identify exactly one CTF outcome"
    );
    anyhow::ensure!(
        a.checked_add(b) == Some(denominator),
        "invalid binary CTF payouts"
    );
    let numerator = if zero { a } else { b };
    let payout = rust_decimal::Decimal::from_str(&numerator.to_string())?
        / rust_decimal::Decimal::from_str(&denominator.to_string())?;
    Ok(Some((if zero { 0 } else { 1 }, payout)))
}

fn raw_decimal(value: U256) -> Result<rust_decimal::Decimal> {
    Ok(rust_decimal::Decimal::from_str(&value.to_string())?
        / rust_decimal::Decimal::from(1_000_000))
}

/// Read confirmed receipts rather than assuming response amounts include fees.
/// Only transfers involving this wallet are counted. Ambiguous multi-asset
/// transactions are rejected so another strategy's transfers cannot be attributed.
pub async fn winner_receipt_amounts(
    hashes: &[B256],
    wallet: Address,
    token: U256,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal)> {
    let provider = ProviderBuilder::new().connect(RPC_URL_DEFAULT).await?;
    let mut cash = rust_decimal::Decimal::ZERO;
    let mut shares = rust_decimal::Decimal::ZERO;
    let mut seen = std::collections::HashSet::new();
    for hash in hashes {
        if !seen.insert(*hash) {
            continue;
        }
        let receipt = provider
            .get_transaction_receipt(*hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("winner receipt unavailable: {hash}"))?;
        anyhow::ensure!(receipt.status(), "winner receipt reverted: {hash}");
        for log in receipt.inner.logs() {
            let (dc, ds) = winner_transfer_delta(
                log.address(),
                log.topics(),
                &log.data().data,
                wallet,
                token,
            )?;
            cash += dc;
            shares += ds;
        }
    }
    Ok((cash, shares))
}

fn winner_transfer_delta(
    address: Address,
    topics: &[B256],
    data: &[u8],
    wallet: Address,
    token: U256,
) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal)> {
    use rust_decimal::Decimal;
    let mut cash = Decimal::ZERO;
    let mut shares = Decimal::ZERO;
    let word = |index: usize| -> Result<U256> {
        let start = index
            .checked_mul(32)
            .ok_or_else(|| anyhow::anyhow!("transfer offset overflow"))?;
        let end = start
            .checked_add(32)
            .ok_or_else(|| anyhow::anyhow!("transfer offset overflow"))?;
        Ok(U256::from_be_slice(
            data.get(start..end)
                .ok_or_else(|| anyhow::anyhow!("truncated transfer"))?,
        ))
    };
    let is_wallet = |topic: &B256| topic.as_slice()[12..] == wallet.as_slice()[..];
    if (address == PUSD_POLYGON || address == USDC_POLYGON)
        && topics.first() == Some(&keccak256(b"Transfer(address,address,uint256)"))
    {
        anyhow::ensure!(topics.len() == 3, "invalid collateral transfer");
        let amount = raw_decimal(word(0)?)?;
        if is_wallet(&topics[1]) {
            cash += amount;
        }
        if is_wallet(&topics[2]) {
            cash -= amount;
        }
    } else if address == CTF_POLYGON
        && topics.len() == 4
        && (is_wallet(&topics[2]) || is_wallet(&topics[3]))
    {
        let mut add = |id: U256, amount: U256| -> Result<()> {
            anyhow::ensure!(
                id == token,
                "ambiguous receipt contains another wallet outcome"
            );
            let amount = raw_decimal(amount)?;
            if is_wallet(&topics[3]) {
                shares += amount;
            }
            if is_wallet(&topics[2]) {
                shares -= amount;
            }
            Ok(())
        };
        if topics[0] == keccak256(b"TransferSingle(address,address,address,uint256,uint256)") {
            add(word(0)?, word(1)?)?;
        } else if topics[0]
            == keccak256(b"TransferBatch(address,address,address,uint256[],uint256[])")
        {
            let ids: usize = word(0)?.try_into()?;
            let values: usize = word(1)?.try_into()?;
            anyhow::ensure!(
                ids.is_multiple_of(32) && values.is_multiple_of(32),
                "unaligned batch transfer"
            );
            let (ids, values) = (ids / 32, values / 32);
            let count: usize = word(ids)?.try_into()?;
            anyhow::ensure!(
                count <= data.len() / 32 && word(values)? == U256::from(count),
                "invalid batch transfer length"
            );
            for index in 0..count {
                add(word(ids + 1 + index)?, word(values + 1 + index)?)?;
            }
        }
    }
    Ok((cash, shares))
}

#[cfg(test)]
mod winner_receipt_tests {
    use super::*;
    use rust_decimal_macros::dec;
    fn topic(address: Address) -> B256 {
        let mut bytes = [0u8; 32];
        bytes[12..].copy_from_slice(address.as_slice());
        B256::from(bytes)
    }
    #[test]
    fn winner_accounts_for_cash_fees_and_net_received_shares() {
        let wallet = Address::from([1u8; 20]);
        let other = Address::from([2u8; 20]);
        let cash = winner_transfer_delta(
            PUSD_POLYGON,
            &[
                keccak256(b"Transfer(address,address,uint256)"),
                topic(wallet),
                topic(other),
            ],
            &U256::from(970_000u64).to_be_bytes::<32>(),
            wallet,
            U256::from(42),
        )
        .unwrap();
        let mut data = U256::from(42).to_be_bytes::<32>().to_vec();
        data.extend(U256::from(997_900u64).to_be_bytes::<32>());
        let shares = winner_transfer_delta(
            CTF_POLYGON,
            &[
                keccak256(b"TransferSingle(address,address,address,uint256,uint256)"),
                topic(other),
                topic(other),
                topic(wallet),
            ],
            &data,
            wallet,
            U256::from(42),
        )
        .unwrap();
        assert_eq!(cash.0, dec!(0.97));
        assert_eq!(shares.1, dec!(0.9979));
        assert_eq!(shares.1 - cash.0, dec!(0.0279));
    }
    #[test]
    fn winner_rejects_unrelated_outcomes_and_truncated_transfers() {
        let wallet = Address::from([1u8; 20]);
        let other = Address::from([2u8; 20]);
        let topics = [
            keccak256(b"TransferSingle(address,address,address,uint256,uint256)"),
            topic(other),
            topic(other),
            topic(wallet),
        ];
        assert!(winner_transfer_delta(CTF_POLYGON, &topics, &[], wallet, U256::from(42)).is_err());
        let mut data = U256::from(43).to_be_bytes::<32>().to_vec();
        data.extend(U256::from(1).to_be_bytes::<32>());
        assert!(
            winner_transfer_delta(CTF_POLYGON, &topics, &data, wallet, U256::from(42)).is_err()
        );
    }
    #[test]
    fn winner_decodes_batch_transfers_with_checked_offsets() {
        let wallet = Address::from([1u8; 20]);
        let other = Address::from([2u8; 20]);
        let topics = [
            keccak256(b"TransferBatch(address,address,address,uint256[],uint256[])"),
            topic(other),
            topic(other),
            topic(wallet),
        ];
        let mut data = Vec::new();
        for value in [64u64, 128, 1, 42, 1, 1_000_000] {
            data.extend(U256::from(value).to_be_bytes::<32>());
        }
        assert_eq!(
            winner_transfer_delta(CTF_POLYGON, &topics, &data, wallet, U256::from(42))
                .unwrap()
                .1,
            dec!(1)
        );
        data[31] = 65;
        assert!(
            winner_transfer_delta(CTF_POLYGON, &topics, &data, wallet, U256::from(42)).is_err()
        );
    }
}
