//! polypulse library: shared modules for the main binary and test binaries.

mod adapter_auth;
pub mod clob_v2;
mod deposit_wallet_relay;
mod network_retry;
mod proxy_relay;
mod pusd_wrap;
mod wallet_kind;

pub use clob_v2::{
    create_authenticated_clob_client, parse_signature_type, v1_address_to_v2,
    AuthenticatedClobClient, CLOB_API_URL_DEFAULT,
};

pub use proxy_relay::{
    CTF_COLLATERAL_ADAPTER, CTF_POLYGON, NEG_RISK_ADAPTER, NEG_RISK_COLLATERAL_ADAPTER,
    PROXY_MERGE_PUSD_GAS, PUSD_POLYGON, RPC_URL_DEFAULT, USDC_POLYGON,
};

pub mod copy_trading;
pub mod maker_liquidity;
pub mod merge;
pub mod positions;
pub mod redeem;
pub mod ui;
