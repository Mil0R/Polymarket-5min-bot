//! Connection health and bounded reconnect policy for authenticated streams.

use std::time::Duration;

pub struct ConnectionSupervisor {
    healthy: bool,
    current_delay: Duration,
    minimum_delay: Duration,
    maximum_delay: Duration,
    stable_after: Duration,
    awaiting_recovery: bool,
}

impl ConnectionSupervisor {
    pub fn new(minimum_delay: Duration, maximum_delay: Duration, stable_after: Duration) -> Self {
        Self {
            healthy: false,
            current_delay: minimum_delay,
            minimum_delay,
            maximum_delay,
            stable_after,
            awaiting_recovery: false,
        }
    }

    /// Returns true when this ready state recovered a prior connection failure.
    pub fn subscription_ready(&mut self) -> bool {
        self.healthy = true;
        std::mem::take(&mut self.awaiting_recovery)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    pub fn connect_failed(&mut self) -> Duration {
        self.healthy = false;
        self.awaiting_recovery = true;
        self.take_delay()
    }

    pub fn disconnected(&mut self, connected_for: Duration) -> Duration {
        self.healthy = false;
        self.awaiting_recovery = true;
        if connected_for >= self.stable_after {
            self.current_delay = self.minimum_delay;
        }
        self.take_delay()
    }

    pub fn normal_session_end(&mut self) {
        self.healthy = false;
        self.current_delay = self.minimum_delay;
    }

    fn take_delay(&mut self) -> Duration {
        let delay = self.current_delay;
        self.current_delay = (self.current_delay * 2).min(self.maximum_delay);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_pauses_entries_and_uses_bounded_backoff() {
        let mut supervisor = ConnectionSupervisor::new(
            Duration::from_secs(1),
            Duration::from_secs(4),
            Duration::from_secs(30),
        );
        assert!(!supervisor.subscription_ready());
        assert!(supervisor.is_healthy());
        assert_eq!(
            supervisor.disconnected(Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert!(!supervisor.is_healthy());
        assert_eq!(supervisor.connect_failed(), Duration::from_secs(2));
        assert_eq!(supervisor.connect_failed(), Duration::from_secs(4));
        assert_eq!(supervisor.connect_failed(), Duration::from_secs(4));
        assert_eq!(
            supervisor.disconnected(Duration::from_secs(30)),
            Duration::from_secs(1)
        );
        assert!(supervisor.subscription_ready());
        assert!(!supervisor.subscription_ready());
    }
}
