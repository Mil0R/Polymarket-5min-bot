use anyhow::{Context, Result};
use dotenvy::dotenv;
use polypulse::wallet_copy_trading::{
    ScoutProfile, WalletAuditConfig, WalletAuditReport, WalletAuditor,
};
use std::env;
use std::path::PathBuf;

const HELP: &str = r#"wallet-copy-audit — read-only due diligence for explicit Polymarket wallets

USAGE:
  wallet_copy_audit ADDRESS[,ADDRESS...] [OPTIONS]
  wallet_copy_audit --wallets ADDRESS[,ADDRESS...] [OPTIONS]

OPTIONS:
  --profile conservative|research  Admission preset (default: conservative)
  --lookback-days N                Public history lookback (default: 90)
  --sync-window-secs N             Cross-wallet timing window (default: 15)
  --onchain-sample N               Recent receipts per wallet, 0 disables (default: 200)
  --rpc-url URL                    Polygon JSON-RPC; defaults to WALLET_COPY_TRADING_ONCHAIN_RPC_URL
  --output FILE                    Append-only NDJSON (default: results/wallet-copy-audits/audits.ndjson)
  -h, --help

The auditor never reads a trading key and never submits orders. PAPER_CANDIDATE
means eligible for dry-run validation, not approved for live trading.
"#;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    install_tls_provider();
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print!("{HELP}");
        return Ok(());
    }

    let mut config = WalletAuditConfig::default();
    let mut positional_wallets = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        let value = || {
            args.get(index + 1)
                .with_context(|| format!("missing value for {argument}"))
        };
        match argument.as_str() {
            "--wallets" => {
                config.addresses.extend(list(value()?));
                index += 2;
            }
            "--profile" => {
                config.profile = ScoutProfile::parse(value()?)?;
                index += 2;
            }
            "--lookback-days" => {
                config.lookback_days = parse(value()?, argument)?;
                index += 2;
            }
            "--sync-window-secs" => {
                config.synchronization_window_secs = parse(value()?, argument)?;
                index += 2;
            }
            "--onchain-sample" => {
                config.onchain_receipt_sample = parse(value()?, argument)?;
                index += 2;
            }
            "--rpc-url" => {
                config.polygon_rpc_url = Some(value()?.to_string());
                index += 2;
            }
            "--output" => {
                config.output_file = PathBuf::from(value()?);
                index += 2;
            }
            unknown if !unknown.starts_with('-') && positional_wallets.is_none() => {
                positional_wallets = Some(unknown);
                index += 1;
            }
            unknown => anyhow::bail!("unknown argument '{unknown}'\n\n{HELP}"),
        }
    }
    if let Some(wallets) = positional_wallets {
        config.addresses.extend(list(wallets));
    }
    let output = config.output_file.clone();
    println!(
        "开始审计 {} 个 Polymarket 地址；profile={:?}, lookback={} days",
        config.addresses.len(),
        config.profile,
        config.lookback_days
    );
    let report = WalletAuditor::new(config)?.run().await?;
    print_report(&report);
    println!("\n完整审计记录已追加到：{}", output.display());
    Ok(())
}

fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse<T>(value: &str, flag: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| anyhow::anyhow!("invalid {flag} value '{value}': {error}"))
}

fn print_report(report: &WalletAuditReport) {
    println!("\n========== 钱包跟单审计结论 ==========");
    for resolution in &report.address_resolutions {
        if resolution.proxy_wallet_resolved {
            println!(
                "地址解析：{} -> proxy wallet {}",
                resolution.input_address, resolution.audit_address
            );
        }
    }
    for wallet in &report.wallets {
        println!(
            "\n{}  verdict={}  score={:.2}  integrity_risk={:.2}",
            wallet.address, wallet.verdict, wallet.overall_score, wallet.integrity_risk_score
        );
        if let Some(scout) = &wallet.scout {
            println!(
                "  PnL=${:.2}  ROI={:.2}%  PF={:.2}  win_rate={:.2}%  drawdown={:.2}%",
                scout.total_pnl_usdc,
                scout.net_roi * 100.0,
                scout.profit_factor,
                scout.win_rate * 100.0,
                scout.max_drawdown * 100.0,
            );
            println!(
                "  resolved_markets={}  active_days={}  live_execution_fit={}",
                scout.resolved_markets,
                scout.recent_active_days,
                scout
                    .live_execution_fit
                    .map(|value| format!("{:.2}%", value * 100.0))
                    .unwrap_or_else(|| "未测量".into())
            );
        } else {
            println!("  基础盈利评价不可用");
        }
        println!(
            "  trades={}  markets={}  observable_volume=${:.2}  rewards=${:.2}  reward_dependency={:.2}%",
            wallet.public_trades,
            wallet.distinct_markets,
            wallet.observable_trade_volume_usdc,
            wallet.rewards_and_rebates_usdc,
            wallet.reward_dependency_ratio * 100.0,
        );
        println!(
            "  onchain_receipts={}/{}  matched_fills={}  counterparty_hhi={}",
            wallet.onchain_receipts_loaded,
            wallet.onchain_receipts_requested,
            wallet.onchain_order_fills_matched,
            wallet
                .counterparty_fill_count_hhi
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "未测量".into())
        );
        if !wallet.hard_rejection_reasons.is_empty() {
            println!("  拒绝原因：{}", wallet.hard_rejection_reasons.join(" | "));
        }
        if !wallet.warnings.is_empty() {
            println!("  警告：{}", wallet.warnings.join(" | "));
        }
    }
    if !report.address_links.is_empty() {
        println!("\n---------- 输入地址关联检测 ----------");
        for link in &report.address_links {
            println!(
                "{} <-> {}  {}  suspicion={:.2}  shared_tx={}  direct_fills={}  sync_same={}  sync_opposite={}",
                link.address_a,
                link.address_b,
                link.classification,
                link.suspicion_score,
                link.shared_transaction_hashes,
                link.direct_onchain_fill_links,
                link.synchronized_same_direction_trades,
                link.synchronized_opposite_direction_trades,
            );
        }
    }
    if !report.partial_failures.is_empty() {
        println!(
            "\n部分数据失败（不能解释为零风险）：{}",
            report.partial_failures.join(" | ")
        );
    }
    println!(
        "\n历史延迟跟单收益：{}",
        report.methodology.historical_delay_pnl_status
    );
    println!("链上对手方覆盖：{}", report.methodology.counterparty_status);
}
