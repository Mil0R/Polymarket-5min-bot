//! Per-window ownership and trading gate.

use polymarket_client_sdk::types::{B256, U256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

pub struct WindowScope {
    condition_ids: HashSet<B256>,
    token_ids: HashSet<U256>,
    trading_paused: AtomicBool,
}

impl WindowScope {
    pub fn new(markets: &HashMap<B256, (U256, U256)>) -> Self {
        Self {
            condition_ids: markets.keys().copied().collect(),
            token_ids: markets.values().flat_map(|(yes, no)| [*yes, *no]).collect(),
            trading_paused: AtomicBool::new(false),
        }
    }

    pub fn pause_trading(&self) {
        self.trading_paused.store(true, Ordering::Release);
    }

    pub fn trading_paused(&self) -> bool {
        self.trading_paused.load(Ordering::Acquire)
    }

    pub fn condition_ids(&self) -> HashSet<B256> {
        self.condition_ids.clone()
    }

    pub fn token_ids(&self) -> HashSet<U256> {
        self.token_ids.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_owns_only_current_window_assets_and_pause_is_sticky() {
        let condition = B256::repeat_byte(1);
        let mut markets = HashMap::new();
        markets.insert(condition, (U256::from(10), U256::from(11)));
        let scope = WindowScope::new(&markets);

        assert!(scope.condition_ids().contains(&condition));
        assert_eq!(scope.token_ids().len(), 2);
        assert!(!scope.trading_paused());
        scope.pause_trading();
        assert!(scope.trading_paused());
    }
}
