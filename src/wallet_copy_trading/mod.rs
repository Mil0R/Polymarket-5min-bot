pub mod audit;
pub mod consensus;
pub mod coordinator;
pub mod execution;
pub mod exit_engine;
pub mod feed;
pub mod journal;
pub mod ladder_exit;
pub mod live_executor;
pub mod market_filter;
pub mod onchain_feed;
pub mod quality;
pub mod risk;
pub mod runtime;
pub mod scout;
pub mod settlement;
pub mod signal_store;
pub mod types;
pub mod whitelist;

pub use audit::{WalletAuditConfig, WalletAuditReport, WalletAuditor};
pub use consensus::ConsensusEngine;
pub use coordinator::WalletCopyTradingCoordinator;
pub use execution::SmartExecutionEngine;
pub use exit_engine::ExitEngine;
pub use feed::{
    BookFetcher, DomainClassifier, MarketMetadataFetcher, TradeFeed, WalletHistoryFetcher,
};
pub use journal::{init_wallet_copy_trading_logging, CopyTradeJournal};
pub use ladder_exit::{ladder_size, ladder_target_price, LadderExitConfig, LadderExitTracker};
pub use live_executor::{CopyTradeExecutor, OrderExecutionOutcome};
pub use market_filter::MarketFilterEngine;
pub use onchain_feed::OnChainTradeFeed;
pub use quality::{WalletQualityPolicy, WalletQualitySnapshot, WalletQualityTracker};
pub use risk::RiskController;
pub use runtime::{WalletCopyTradingRuntime, WalletCopyTradingRuntimeSnapshot};
pub use scout::{CandidateScout, ScoutConfig, ScoutProfile, ScoutReport};
pub use settlement::{determine_resolved_outcome, GammaSettlementSource, SettlementMonitor};
pub use signal_store::ProcessedSignalStore;
pub use types::*;
pub use whitelist::WhitelistManager;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use rust_decimal_macros::dec;

    fn create_test_trade_record(
        trade_id: &str,
        days_ago: i64,
        entry_price: rust_decimal::Decimal,
        pnl: rust_decimal::Decimal,
        is_maker: bool,
        is_arb: bool,
        won: bool,
    ) -> WalletTradeRecord {
        let now = Utc::now();
        let opened_at = now - Duration::days(days_ago);
        let settled_at = Some(opened_at + Duration::hours(24));
        let outcome_bought = OutcomeSide::Yes;
        let resolved_outcome = if won {
            Some(OutcomeSide::Yes)
        } else {
            Some(OutcomeSide::No)
        };

        WalletTradeRecord {
            trade_id: trade_id.to_string(),
            market_id: format!("mkt-{}", trade_id),
            domain: DomainTag::MacroInterestRate,
            outcome_bought,
            entry_price,
            exit_price: Some(if won { dec!(1.0) } else { dec!(0.0) }),
            notional_usdc: dec!(100.0),
            pnl_usdc: pnl,
            return_pct: pnl / dec!(100.0),
            is_maker,
            is_arbitrage_leg: is_arb,
            behavior_data_known: true,
            resolved_outcome,
            opened_at,
            settled_at,
        }
    }

    #[test]
    fn test_module_1_whitelist_funnel_and_blacklists() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config);
        let now = Utc::now();

        // 1. Generate 60 qualified trades spanning 118 days (days_ago: 120 -> 2)
        let mut qualified_trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            let won = i % 3 != 0; // ~66% win rate
            let pnl = if won { dec!(20.0) } else { dec!(-20.0) };
            qualified_trades.push(create_test_trade_record(
                &format!("t-{}", i),
                days_ago,
                dec!(0.50), // avg entry 0.50 within [0.20, 0.80]
                pnl,
                false, // taker
                false, // no arb
                won,
            ));
        }

        let res = manager.evaluate_wallet("0xMasterTrader", &qualified_trades, now);
        assert!(
            res.is_accepted,
            "Qualified trader should be accepted: {:?}",
            res.failure_reasons
        );
        assert_eq!(res.profile.total_settled_events, 60);
        assert!(res.profile.active_span_days >= 90);
        assert_eq!(res.profile.primary_domain, DomainTag::MacroInterestRate);

        // 2. Test Rejection: Insufficient settlements (< 50)
        let few_trades = &qualified_trades[0..30];
        let res_few = manager.evaluate_wallet("0xNewbie", few_trades, now);
        assert!(!res_few.is_accepted);
        assert!(res_few
            .failure_reasons
            .iter()
            .any(|r| r.contains("Insufficient settled events")));

        // 3. Test Rejection: Maker Ratio > 90% (Market Maker bot)
        let mut maker_trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            maker_trades.push(create_test_trade_record(
                &format!("m-{}", i),
                days_ago,
                dec!(0.50),
                dec!(10.0),
                true, // is_maker = true
                false,
                true,
            ));
        }
        let res_maker = manager.evaluate_wallet("0xMakerBot", &maker_trades, now);
        assert!(!res_maker.is_accepted);
        assert!(res_maker
            .failure_reasons
            .iter()
            .any(|r| r.contains("maker bot")));

        // 4. Test Rejection: Survivor Bias (Single trade profit > 25%)
        let mut survivor_trades = Vec::new();
        for i in 0..59 {
            let days_ago = 120 - (i as i64 * 2);
            survivor_trades.push(create_test_trade_record(
                &format!("s-{}", i),
                days_ago,
                dec!(0.50),
                dec!(1.0), // $1 profit each
                false,
                false,
                true,
            ));
        }
        // One mega profit trade of $500
        survivor_trades.push(create_test_trade_record(
            "s-59",
            1,
            dec!(0.50),
            dec!(500.0),
            false,
            false,
            true,
        ));
        let res_survivor = manager.evaluate_wallet("0xLuckyGambler", &survivor_trades, now);
        assert!(!res_survivor.is_accepted);
        assert!(res_survivor
            .failure_reasons
            .iter()
            .any(|r| r.contains("Single trade profit dominance")));
    }

    #[test]
    fn test_module_1_three_strikes_disqualification() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config);
        let now = Utc::now();

        let mut trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades.push(create_test_trade_record(
                &format!("t-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let res = manager.evaluate_wallet("0xExpert", &trades, now);
        assert!(
            res.is_accepted,
            "Expert should be accepted: {:?}",
            res.failure_reasons
        );
        manager.upsert_wallet(res.profile);

        let (auth, tier) =
            manager.is_authorized_in_domain("0xExpert", &DomainTag::MacroInterestRate);
        assert!(auth);
        assert_eq!(tier, Some(WalletTier::TierA));

        // 3 consecutive major direction failures
        manager.record_direction_result("0xExpert", true); // Strike 1
        manager.record_direction_result("0xExpert", true); // Strike 2
        let (auth2, _) = manager.is_authorized_in_domain("0xExpert", &DomainTag::MacroInterestRate);
        assert!(auth2, "Should still be authorized on strike 2");

        manager.record_direction_result("0xExpert", true); // Strike 3 -> Disqualified!
        let (auth3, tier3) =
            manager.is_authorized_in_domain("0xExpert", &DomainTag::MacroInterestRate);
        assert!(!auth3, "Should be disqualified after 3 strikes");
        assert_eq!(tier3, Some(WalletTier::Blacklisted));
    }

    #[test]
    fn test_module_2_market_filter() {
        let config = WalletCopyTradingConfig::default();
        let filter = MarketFilterEngine::new(config);
        let now = Utc::now();

        let valid_market = MarketMetadata {
            market_id: "mkt-valid".to_string(),
            condition_id: "0xcond1".to_string(),
            question: "Will Fed cut rates in Sept?".to_string(),
            domain: DomainTag::MacroInterestRate,
            outcome_count: 2,
            is_neg_risk: false,
            settlement_time: now + Duration::hours(120), // 5 days > 72h
            volume_24h_usdc: dec!(100_000.0),            // > $50k
            is_active: true,
        };

        let valid_book = OrderBookSnapshot {
            market_id: "mkt-valid".to_string(),
            token_id: "tok-1".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.505),
                size: dec!(1000.0),
            }],
            timestamp: now,
        };

        // 1. Valid market passes
        assert!(filter
            .validate_market(&valid_market, &valid_book, now)
            .is_ok());

        // 2. Reject multi-outcome complex market (outcomes >= 3)
        let multi_market = MarketMetadata {
            outcome_count: 5,
            ..valid_market.clone()
        };
        assert!(filter
            .validate_market(&multi_market, &valid_book, now)
            .is_err());

        // 3. Reject short-term expiry (< 72h)
        let short_market = MarketMetadata {
            settlement_time: now + Duration::hours(24), // only 24h left
            ..valid_market.clone()
        };
        assert!(filter
            .validate_market(&short_market, &valid_book, now)
            .is_err());

        // 4. Reject wide spread (> 2%)
        let wide_book = OrderBookSnapshot {
            bids: vec![BookLevel {
                price: dec!(0.45),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.55),
                size: dec!(1000.0),
            }], // spread = 0.10 / 0.50 = 20%
            ..valid_book.clone()
        };
        assert!(filter
            .validate_market(&valid_market, &wide_book, now)
            .is_err());
    }

    #[test]
    fn test_module_3_execution_pricing_and_anti_dump_depth() {
        let config = WalletCopyTradingConfig::default();
        let execution = SmartExecutionEngine::new(config);
        let now = Utc::now();

        let consensus = ConsensusSignal {
            consensus_id: "cs-1".to_string(),
            market_id: "mkt-1".to_string(),
            token_id: "tok-1".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            contributing_wallets: vec!["0xW1".to_string(), "0xW2".to_string()],
            consensus_score: dec!(0.9),
            target_vwap: dec!(0.50),
            created_at: now,
        };

        // Book with 5 ask levels, total notional = 5 * (0.505 * 200) = $505.00
        let book = OrderBookSnapshot {
            market_id: "mkt-1".to_string(),
            token_id: "tok-1".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(500.0),
            }],
            asks: vec![
                BookLevel {
                    price: dec!(0.505),
                    size: dec!(200.0),
                }, // $101
                BookLevel {
                    price: dec!(0.506),
                    size: dec!(200.0),
                }, // $101.2
                BookLevel {
                    price: dec!(0.507),
                    size: dec!(200.0),
                }, // $101.4
                BookLevel {
                    price: dec!(0.508),
                    size: dec!(200.0),
                }, // $101.6
                BookLevel {
                    price: dec!(0.509),
                    size: dec!(200.0),
                }, // $101.8
            ],
            timestamp: now,
        };

        // Allocated budget is $1000, but Top-5 depth is ~$507, so 10% limit = ~$50.70
        let prop = execution
            .build_buy_proposal(&consensus, &book, dec!(1000.0), now)
            .unwrap();
        assert!(
            prop.notional_usdc <= dec!(52.0),
            "Notional must be clipped by 10% Top-5 depth: {}",
            prop.notional_usdc
        );
        assert_eq!(prop.limit_price, dec!(0.507));
        assert_eq!(prop.ttl_seconds, 45);

        // Test chasing high rejection when Best Ask exceeds price offset / slippage
        let expensive_book = OrderBookSnapshot {
            asks: vec![BookLevel {
                price: dec!(0.55),
                size: dec!(1000.0),
            }], // > 0.50 + 0.015
            ..book.clone()
        };
        let err = execution.build_buy_proposal(&consensus, &expensive_book, dec!(100.0), now);
        assert!(
            err.is_err(),
            "Must reject order when best ask is too high above target VWAP"
        );
    }

    #[test]
    fn test_module_4_risk_drawdown_breaker_and_exposure_cap() {
        let config = WalletCopyTradingConfig::default();
        let now = Utc::now();
        let mut risk = RiskController::new(config, dec!(10_000.0), now); // $10,000 NAV

        // 1. Standard 2% allocation = $200
        let alloc = risk.allocate_trade_budget("mkt-1", now).unwrap();
        assert_eq!(alloc, dec!(200.0));

        // 2. Record full 10% exposure in mkt-1 ($1000)
        risk.record_position_fill(
            "mkt-1",
            "tok-1",
            &OutcomeSide::Yes,
            dec!(2000.0),
            dec!(0.50),
        ); // $1,000 exposure

        // Exceeding 10% exposure cap should fail
        let err_cap = risk.allocate_trade_budget("mkt-1", now);
        assert!(
            err_cap.is_err(),
            "Must reject additional allocation when single market cap (10%) is met"
        );

        // 2b. Portfolio-wide total exposure cap (default 30% of NAV): after $5000
        //     of aggregate exposure is reached, further allocation must fail.
        risk.record_position_fill(
            "mkt-1",
            "tok-1b",
            &OutcomeSide::No,
            dec!(8000.0),
            dec!(0.50),
        ); // +$4000 more exposure
        let err_portfolio = risk.allocate_trade_budget("mkt-2", now);
        assert!(
            err_portfolio.is_err(),
            "Must reject allocation when portfolio total exposure cap is met"
        );

        // 3. Test 5% Daily Drawdown Circuit Breaker
        // Buy $1000 of position on a fresh risk controller with $10,000 NAV
        let mut breaker_risk =
            RiskController::new(WalletCopyTradingConfig::default(), dec!(10_000.0), now);
        breaker_risk.record_position_fill(
            "mkt-loss",
            "tok-1",
            &OutcomeSide::Yes,
            dec!(1000.0),
            dec!(1.0),
        ); // spent $1000 cash, holding 1000 shares
           // Mark down position to $0.40 -> lost $600 -> NAV = $9000 cash + $400 position = $9400 (6% loss)
        breaker_risk.update_mark_price("tok-1", dec!(0.40));
        let broken = breaker_risk.check_and_apply_drawdown_breaker(now);
        assert!(
            broken,
            "Circuit breaker must trigger when daily drawdown >= 5%"
        );
    }

    #[tokio::test]
    async fn test_module_5_and_end_to_end_coordinator() {
        let config = WalletCopyTradingConfig::default();
        let now = Utc::now();
        let coordinator = WalletCopyTradingCoordinator::new(config, dec!(10_000.0), now);

        // 1. Seed two whitelisted expert wallets in MacroInterestRate domain with >= 90 days span
        let mut trades1 = Vec::new();
        let mut trades2 = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades1.push(create_test_trade_record(
                &format!("w1-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
            trades2.push(create_test_trade_record(
                &format!("w2-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let w1_res = coordinator
            .whitelist()
            .evaluate_wallet("0xAlpha", &trades1, now);
        let w2_res = coordinator
            .whitelist()
            .evaluate_wallet("0xBeta", &trades2, now);
        assert!(
            w1_res.is_accepted,
            "Alpha should be accepted: {:?}",
            w1_res.failure_reasons
        );
        assert!(
            w2_res.is_accepted,
            "Beta should be accepted: {:?}",
            w2_res.failure_reasons
        );
        coordinator.whitelist().upsert_wallet(w1_res.profile);
        coordinator.whitelist().upsert_wallet(w2_res.profile);

        let market = MarketMetadata {
            market_id: "mkt-e2e".to_string(),
            condition_id: "0xcond_e2e".to_string(),
            question: "Will ECB cut rates?".to_string(),
            domain: DomainTag::MacroInterestRate,
            outcome_count: 2,
            is_neg_risk: false,
            settlement_time: now + Duration::hours(100),
            volume_24h_usdc: dec!(80_000.0),
            is_active: true,
        };

        let book = OrderBookSnapshot {
            market_id: "mkt-e2e".to_string(),
            token_id: "tok-e2e".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.505),
                size: dec!(2000.0),
            }],
            timestamp: now,
        };

        // 2. First wallet 0xAlpha enters: consensus NOT met yet (waiting for 2nd wallet)
        let sig1 = SignalEvent {
            signal_id: "sig-1".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-e2e".to_string(),
            token_id: "tok-e2e".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: None,
            timestamp: now,
        };
        let prop1 = coordinator
            .process_signal(sig1, Some(&market), &book, now)
            .await
            .unwrap();
        assert!(
            prop1.is_none(),
            "Single A-tier wallet must not trigger execution alone"
        );

        // 3. Second wallet 0xBeta enters within 12h: consensus ACHIEVED -> Generates proposal!
        let sig2 = SignalEvent {
            signal_id: "sig-2".to_string(),
            source_wallet: "0xBeta".to_string(),
            market_id: "mkt-e2e".to_string(),
            token_id: "tok-e2e".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::minutes(30),
        };
        let prop2 = coordinator
            .process_signal(sig2, Some(&market), &book, now + Duration::minutes(30))
            .await
            .unwrap();
        assert!(
            prop2.is_some(),
            "Consensus achieved: proposal must be generated"
        );
        let proposal = prop2.unwrap();
        assert_eq!(proposal.side, OrderSide::Buy);
        assert_eq!(proposal.limit_price, dec!(0.505));
        assert!(proposal.notional_usdc > dec!(0.0));

        // 4. Place order and simulate fill
        coordinator.on_order_placed("order-101".to_string(), proposal.clone(), now);
        coordinator
            .on_order_fill(
                "order-101",
                "mkt-e2e",
                "tok-e2e",
                proposal.shares,
                proposal.limit_price,
                now,
            )
            .await;

        // 5. Target wallet exits: trigger Exit Mirroring sell proposal
        let exit_sig = SignalEvent {
            signal_id: "sig-exit".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-e2e".to_string(),
            token_id: "tok-e2e".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::CloseSell,
            target_vwap: dec!(0.60),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(300.0),
            target_reduction_pct: Some(dec!(0.25)),
            timestamp: now + Duration::hours(2),
        };
        let exit_prop = coordinator
            .process_signal(exit_sig, Some(&market), &book, now + Duration::hours(2))
            .await
            .unwrap();
        assert!(
            exit_prop.is_some(),
            "Exit mirroring must generate Sell proposal"
        );
        let exit_proposal = exit_prop.unwrap();
        assert_eq!(exit_proposal.side, OrderSide::Sell);
        assert_eq!(
            exit_proposal.token_id, "tok-e2e",
            "Exit order token must come from OUR position, not the signal"
        );
        assert_eq!(exit_proposal.outcome, OutcomeSide::Yes);
        // Target sold 25% of its position, so we reduce 25% too (below depth cap).
        assert_eq!(exit_proposal.shares, proposal.shares * dec!(0.25));
        assert!(exit_proposal.shares <= proposal.shares);
    }

    #[tokio::test]
    async fn test_fix_exit_direction_mismatch_is_blocked() {
        // Regression: a target wallet exiting the OPPOSITE side of the market must
        // never generate a sell proposal against our position.
        let config = WalletCopyTradingConfig::default();
        let now = Utc::now();
        let coordinator = WalletCopyTradingCoordinator::new(config, dec!(10_000.0), now);

        // Seed two whitelisted experts (Alpha + consensus partner Beta)
        let mut trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades.push(create_test_trade_record(
                &format!("w-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let w_res = coordinator
            .whitelist()
            .evaluate_wallet("0xAlpha", &trades, now);
        assert!(w_res.is_accepted);
        coordinator.whitelist().upsert_wallet(w_res.profile);
        let mut trades_b = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades_b.push(create_test_trade_record(
                &format!("b-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let b_res = coordinator
            .whitelist()
            .evaluate_wallet("0xBeta", &trades_b, now);
        assert!(b_res.is_accepted);
        coordinator.whitelist().upsert_wallet(b_res.profile);

        let market = MarketMetadata {
            market_id: "mkt-dir".to_string(),
            condition_id: "0xcond_dir".to_string(),
            question: "Will X happen?".to_string(),
            domain: DomainTag::MacroInterestRate,
            outcome_count: 2,
            is_neg_risk: false,
            settlement_time: now + Duration::hours(100),
            volume_24h_usdc: dec!(80_000.0),
            is_active: true,
        };
        let book = OrderBookSnapshot {
            market_id: "mkt-dir".to_string(),
            token_id: "tok-yes".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.505),
                size: dec!(2000.0),
            }],
            timestamp: now,
        };

        // We buy YES via consensus (2 wallets)
        let sig1 = SignalEvent {
            signal_id: "sig-a".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-dir".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now,
        };
        let _ = coordinator
            .process_signal(sig1, Some(&market), &book, now)
            .await
            .unwrap();
        let sig2 = SignalEvent {
            signal_id: "sig-b".to_string(),
            source_wallet: "0xBeta".to_string(),
            market_id: "mkt-dir".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::minutes(5),
        };
        let prop = coordinator
            .process_signal(sig2, Some(&market), &book, now + Duration::minutes(5))
            .await
            .unwrap()
            .unwrap();
        coordinator.on_order_placed("order-dir".to_string(), prop.clone(), now);
        coordinator
            .on_order_fill(
                "order-dir",
                "mkt-dir",
                "tok-yes",
                prop.shares,
                prop.limit_price,
                now,
            )
            .await;

        // Target wallet now exits the NO side (we never held NO): must NOT mirror
        let no_exit = SignalEvent {
            signal_id: "sig-exit-no".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-dir".to_string(),
            token_id: "tok-no".to_string(),
            outcome: OutcomeSide::No,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::CloseSell,
            target_vwap: dec!(0.60),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(300.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::hours(2),
        };
        let no_exit_prop = coordinator
            .process_signal(no_exit, Some(&market), &book, now + Duration::hours(2))
            .await
            .unwrap();
        assert!(
            no_exit_prop.is_none(),
            "Exit on the opposite side must not be mirrored"
        );

        // Sanity: exit on the YES side (our position) still works
        let yes_exit = SignalEvent {
            signal_id: "sig-exit-yes".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-dir".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::CloseSell,
            target_vwap: dec!(0.60),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(300.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::hours(3),
        };
        let yes_exit_prop = coordinator
            .process_signal(yes_exit, Some(&market), &book, now + Duration::hours(3))
            .await
            .unwrap();
        assert!(
            yes_exit_prop.is_some(),
            "Exit matching our held direction must be mirrored"
        );
    }

    #[tokio::test]
    async fn test_fix_exit_unauthorized_wallet_blocked_and_sell_fill() {
        let config = WalletCopyTradingConfig::default();
        let now = Utc::now();
        let coordinator = WalletCopyTradingCoordinator::new(config, dec!(10_000.0), now);

        // Whitelisted Alpha + consensus partner Beta + unlisted Eve
        let mut trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades.push(create_test_trade_record(
                &format!("w-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let w_res = coordinator
            .whitelist()
            .evaluate_wallet("0xAlpha", &trades, now);
        assert!(w_res.is_accepted);
        coordinator.whitelist().upsert_wallet(w_res.profile);
        let mut trades_b = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            trades_b.push(create_test_trade_record(
                &format!("b-{}", i),
                days_ago,
                dec!(0.50),
                dec!(20.0),
                false,
                false,
                true,
            ));
        }
        let b_res = coordinator
            .whitelist()
            .evaluate_wallet("0xBeta", &trades_b, now);
        assert!(b_res.is_accepted);
        coordinator.whitelist().upsert_wallet(b_res.profile);

        let market = MarketMetadata {
            market_id: "mkt-auth".to_string(),
            condition_id: "0xcond_auth".to_string(),
            question: "Will Y happen?".to_string(),
            domain: DomainTag::MacroInterestRate,
            outcome_count: 2,
            is_neg_risk: false,
            settlement_time: now + Duration::hours(100),
            volume_24h_usdc: dec!(80_000.0),
            is_active: true,
        };
        let book = OrderBookSnapshot {
            market_id: "mkt-auth".to_string(),
            token_id: "tok-yes".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.505),
                size: dec!(2000.0),
            }],
            timestamp: now,
        };

        let mk = |id: &str, wallet: &str, ts: chrono::DateTime<Utc>| SignalEvent {
            signal_id: id.to_string(),
            source_wallet: wallet.to_string(),
            market_id: "mkt-auth".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: None,
            timestamp: ts,
        };

        let _ = coordinator
            .process_signal(mk("s1", "0xAlpha", now), Some(&market), &book, now)
            .await
            .unwrap();
        let prop = coordinator
            .process_signal(
                mk("s2", "0xBeta", now + Duration::minutes(5)),
                Some(&market),
                &book,
                now + Duration::minutes(5),
            )
            .await
            .unwrap()
            .unwrap();
        coordinator.on_order_placed("order-auth".to_string(), prop.clone(), now);
        coordinator
            .on_order_fill(
                "order-auth",
                "mkt-auth",
                "tok-yes",
                prop.shares,
                prop.limit_price,
                now,
            )
            .await;

        // Unauthorized wallet (never evaluated) cannot trigger an exit
        let eve_exit = SignalEvent {
            signal_id: "sig-eve".to_string(),
            source_wallet: "0xEve".to_string(),
            market_id: "mkt-auth".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::CloseSell,
            target_vwap: dec!(0.60),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(300.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::hours(2),
        };
        let eve_prop = coordinator
            .process_signal(eve_exit, Some(&market), &book, now + Duration::hours(2))
            .await
            .unwrap();
        assert!(
            eve_prop.is_none(),
            "Exit from non-whitelisted wallet must be blocked"
        );

        // Authorized wallet exit generates a sell; executing its fill must reduce
        // our position (SELL fill path in risk accounting).
        let alpha_exit = SignalEvent {
            signal_id: "sig-alpha-exit".to_string(),
            source_wallet: "0xAlpha".to_string(),
            market_id: "mkt-auth".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::CloseSell,
            target_vwap: dec!(0.60),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(300.0),
            target_reduction_pct: Some(dec!(1)),
            timestamp: now + Duration::hours(3),
        };
        let exit_prop = coordinator
            .process_signal(alpha_exit, Some(&market), &book, now + Duration::hours(3))
            .await
            .unwrap()
            .unwrap();
        coordinator.on_order_placed("order-exit".to_string(), exit_prop.clone(), now);

        let shares_before = coordinator
            .risk()
            .read()
            .await
            .get_market_exposure("mkt-auth", &OutcomeSide::Yes)
            .unwrap()
            .shares;
        coordinator
            .on_order_fill(
                "order-exit",
                "mkt-auth",
                "tok-yes",
                exit_prop.shares,
                exit_prop.limit_price,
                now + Duration::minutes(1),
            )
            .await;
        let shares_after = coordinator
            .risk()
            .read()
            .await
            .get_market_exposure("mkt-auth", &OutcomeSide::Yes)
            .unwrap()
            .shares;
        assert_eq!(
            shares_before - shares_after,
            exit_prop.shares,
            "Sell fill must reduce the position by the filled size"
        );
    }

    #[test]
    fn test_fix_consensus_bucket_cleared_after_trigger() {
        // Regression: after a consensus fires, the same wallet set must NOT be able
        // to re-trigger a duplicate entry with stale signals after the 6h cooldown.
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config.clone());
        let consensus = ConsensusEngine::new(config, manager.clone());
        let now = Utc::now();

        // Seed whitelisted wallets (Alpha, Beta as first consensus pair, Gamma as fresh source)
        for addr in ["0xAlpha", "0xBeta", "0xGamma"] {
            let mut trades = Vec::new();
            for i in 0..60 {
                let days_ago = 120 - (i as i64 * 2);
                trades.push(create_test_trade_record(
                    &format!("{}-{}", addr, i),
                    days_ago,
                    dec!(0.50),
                    dec!(20.0),
                    false,
                    false,
                    true,
                ));
            }
            let res = manager.evaluate_wallet(addr, &trades, now);
            assert!(res.is_accepted);
            manager.upsert_wallet(res.profile);
        }

        let mk = |id: &str, wallet: &str, ts: chrono::DateTime<Utc>| SignalEvent {
            signal_id: id.to_string(),
            source_wallet: wallet.to_string(),
            market_id: "mkt-c".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: dec!(0.50),
            target_shares: dec!(500.0),
            target_notional_usdc: dec!(250.0),
            target_reduction_pct: None,
            timestamp: ts,
        };

        // First consensus fires
        assert!(consensus
            .ingest_signal(mk("a1", "0xAlpha", now), now)
            .is_none());
        let first = consensus
            .ingest_signal(
                mk("b1", "0xBeta", now + Duration::minutes(5)),
                now + Duration::minutes(5),
            )
            .expect("second source completes consensus");
        consensus.commit_consensus(&first, now + Duration::minutes(5));

        // 7h later, the SAME wallets re-send OpenBuy signals. Bucket was cleared at
        // trigger, so only ONE new signal exists -> no re-trigger without new info.
        let t2 = now + Duration::hours(7);
        assert!(
            consensus
                .ingest_signal(mk("a2", "0xAlpha", t2), t2)
                .is_none(),
            "Stale re-trigger must be blocked after bucket clear"
        );

        // A second distinct partner wallet provides fresh signal -> consensus again
        let t3 = t2 + Duration::minutes(5);
        assert!(
            consensus
                .ingest_signal(mk("c1", "0xGamma", t3), t3)
                .is_some(),
            "Fresh second source must trigger a new consensus"
        );
    }

    #[test]
    fn test_fix_unprofitable_wallet_rejected() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config);
        let now = Utc::now();

        // 60 settled events, 90+ days, entry 0.50, but NET LOSS (breakeven-ish)
        let mut trades = Vec::new();
        for i in 0..60 {
            let days_ago = 120 - (i as i64 * 2);
            let won = i % 2 == 0; // 50/50 coin flip
            let pnl = if won { dec!(25.0) } else { dec!(-25.0) };
            trades.push(create_test_trade_record(
                &format!("u-{}", i),
                days_ago,
                dec!(0.50),
                pnl,
                false,
                false,
                won,
            ));
        }
        let res = manager.evaluate_wallet("0xCoinFlip", &trades, now);
        assert!(
            !res.is_accepted,
            "Coin-flip breakeven wallet must be rejected"
        );
        assert!(res
            .failure_reasons
            .iter()
            .any(|r| r.contains("Not profitable")));
        assert_eq!(
            res.tier,
            WalletTier::Probation,
            "Soft failure should be Probation, not Blacklisted"
        );
    }

    #[test]
    fn test_fix_min_signal_notional_and_vwap_bounds() {
        let config = WalletCopyTradingConfig::default();
        let manager = WhitelistManager::new(config.clone());
        let consensus = ConsensusEngine::new(config.clone(), manager.clone());
        let now = Utc::now();

        for addr in ["0xAlpha", "0xBeta"] {
            let mut trades = Vec::new();
            for i in 0..60 {
                let days_ago = 120 - (i as i64 * 2);
                trades.push(create_test_trade_record(
                    &format!("{}-{}", addr, i),
                    days_ago,
                    dec!(0.50),
                    dec!(20.0),
                    false,
                    false,
                    true,
                ));
            }
            let res = manager.evaluate_wallet(addr, &trades, now);
            manager.upsert_wallet(res.profile);
        }

        let mk = |id: &str,
                  wallet: &str,
                  ts: chrono::DateTime<Utc>,
                  notional: rust_decimal::Decimal,
                  vwap: rust_decimal::Decimal| SignalEvent {
            signal_id: id.to_string(),
            source_wallet: wallet.to_string(),
            market_id: "mkt-m".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            action: SignalAction::OpenBuy,
            target_vwap: vwap,
            target_shares: dec!(500.0),
            target_notional_usdc: notional,
            target_reduction_pct: None,
            timestamp: ts,
        };

        // Dust signal (< $100) is ignored regardless of wallet tier
        assert!(consensus
            .ingest_signal(mk("d1", "0xAlpha", now, dec!(10.0), dec!(0.50)), now)
            .is_none());
        // VWAP outside [0.05, 0.95] is ignored
        assert!(consensus
            .ingest_signal(mk("v1", "0xAlpha", now, dec!(250.0), dec!(0.99)), now)
            .is_none());

        // Execution engine also refuses to build on an out-of-bounds consensus VWAP
        let execution = SmartExecutionEngine::new(config);
        let consensus_sig = ConsensusSignal {
            consensus_id: "cs-bad".to_string(),
            market_id: "mkt-m".to_string(),
            token_id: "tok-yes".to_string(),
            outcome: OutcomeSide::Yes,
            domain: DomainTag::MacroInterestRate,
            contributing_wallets: vec!["0xAlpha".to_string()],
            consensus_score: dec!(0.9),
            target_vwap: dec!(0.99),
            created_at: now,
        };
        let book = OrderBookSnapshot {
            market_id: "mkt-m".to_string(),
            token_id: "tok-yes".to_string(),
            bids: vec![BookLevel {
                price: dec!(0.495),
                size: dec!(1000.0),
            }],
            asks: vec![BookLevel {
                price: dec!(0.505),
                size: dec!(1000.0),
            }],
            timestamp: now,
        };
        assert!(execution
            .build_buy_proposal(&consensus_sig, &book, dec!(200.0), now)
            .is_err());
    }
}
