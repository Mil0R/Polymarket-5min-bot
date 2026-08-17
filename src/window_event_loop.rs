#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowExitReason {
    Rollover,
    UserStreamDisconnected,
    MarketStreamDisconnected,
}

#[derive(Default)]
pub struct WindowEventLoopState {
    exit_reason: Option<WindowExitReason>,
}

impl WindowEventLoopState {
    pub fn exit(&mut self, reason: WindowExitReason) {
        if self.exit_reason.is_none() {
            self.exit_reason = Some(reason);
        }
    }

    pub fn exit_reason(&self) -> Option<WindowExitReason> {
        self.exit_reason
    }

    pub fn requires_reconnect_backoff(&self) -> bool {
        matches!(
            self.exit_reason,
            Some(
                WindowExitReason::UserStreamDisconnected
                    | WindowExitReason::MarketStreamDisconnected
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_first_exit_cause_and_classifies_backoff() {
        let mut state = WindowEventLoopState::default();
        state.exit(WindowExitReason::UserStreamDisconnected);
        state.exit(WindowExitReason::Rollover);
        assert_eq!(
            state.exit_reason(),
            Some(WindowExitReason::UserStreamDisconnected)
        );
        assert!(state.requires_reconnect_backoff());

        let mut rollover = WindowEventLoopState::default();
        rollover.exit(WindowExitReason::Rollover);
        assert!(!rollover.requires_reconnect_backoff());
    }
}
