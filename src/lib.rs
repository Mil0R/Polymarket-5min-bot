//! polypulse library: shared modules for the main binary and test binaries.

extern crate self as polypulse;

mod adapter_auth;
pub mod app;
pub mod clob_v2;
mod config;
mod connection_supervisor;
#[path = "bin/copy_trader.rs"]
mod copy_trader_embedded;
mod deposit_wallet_relay;
mod live_safety;
mod market;
mod monitor;
mod network_retry;
mod observability_server;
mod opportunity_admission;
#[cfg(test)]
mod protocol_contract_tests;
mod proxy_relay;
mod pusd_wrap;
mod risk;
mod runtime_metrics;
mod strategy_gate;
mod strategy_service;
mod trading;
mod utils;
mod wallet_kind;
mod wind_down_service;
mod window;
mod window_event_loop;
mod window_session;
mod window_session_runner;
mod winner_buy;
mod winner_model;

pub use clob_v2::{
    create_authenticated_clob_client, parse_signature_type, v1_address_to_v2,
    AuthenticatedClobClient, CLOB_API_URL_DEFAULT,
};

pub use proxy_relay::{
    CTF_COLLATERAL_ADAPTER, CTF_POLYGON, NEG_RISK_ADAPTER, NEG_RISK_COLLATERAL_ADAPTER,
    PROXY_MERGE_PUSD_GAS, PUSD_POLYGON, RPC_URL_DEFAULT, USDC_POLYGON,
};

pub mod bayesian_arb;
pub mod copy_trading;
pub mod maker_liquidity;
pub mod merge;
pub mod polybacktest;
pub mod positions;
pub mod redeem;
pub mod ui;
