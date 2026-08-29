//! Throwaway probe: verify the on-chain OrderFilled feed decodes real fills.
//! Not intended for production — run once, then delete.
use anyhow::Result;
use chrono::Utc;
use polypulse::wallet_copy_trading::OnChainTradeFeed;

#[tokio::main]
async fn main() -> Result<()> {
    let rpc = std::env::var("WALLET_COPY_TRADING_ONCHAIN_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".to_string());
    let feed = OnChainTradeFeed::new(&rpc).await?;
    let maker: alloy::primitives::Address = std::env::args()
        .nth(1)
        .expect("usage: onchain_probe <maker-address> [from-block]")
        .parse()?;
    if let Some(block) = std::env::args().nth(2) {
        let b: u64 = block.parse()?;
        feed.set_last_block(b);
        eprintln!("[probe] seeded last_block to {b}");
    }
    println!("Probing maker {maker:?} (RPC {rpc})");
    for round in 0..6 {
        let now = Utc::now();
        eprintln!("[probe] round {round} start");
        match feed.poll_wallets(&[maker], now).await {
            Ok(signals) => {
                println!("round {round}: {} signals", signals.len());
                for s in &signals {
                    println!(
                        "  {:?} {} {} shares @ {} = ${} | market {} | {}",
                        s.action,
                        s.outcome,
                        s.target_shares,
                        s.target_vwap,
                        s.target_notional_usdc,
                        &s.market_id[..16],
                        s.domain
                    );
                }
            }
            Err(e) => println!("round {round}: error {e:#}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    }
    Ok(())
}
