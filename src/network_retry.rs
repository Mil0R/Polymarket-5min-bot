use anyhow::Result;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct ReadRetryPolicy {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReadRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(2),
        }
    }
}

pub async fn retry_read<T, F, Fut>(policy: ReadRetryPolicy, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let attempts = policy.max_attempts.max(1);
    let mut delay = policy.initial_delay;
    for attempt in 1..=attempts {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < attempts && is_retryable_read_error(&error) => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(policy.max_delay);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop always returns")
}

fn is_retryable_read_error(error: &anyhow::Error) -> bool {
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return error.is_timeout() || error.is_connect();
    }
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "temporarily unavailable",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn instant_policy() -> ReadRetryPolicy {
        ReadRetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn retries_transient_reads_until_success() {
        let attempts = AtomicUsize::new(0);
        let value = retry_read(instant_policy(), || async {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                anyhow::bail!("503 temporarily unavailable");
            }
            Ok(42)
        })
        .await
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_authentication_or_validation_errors() {
        let attempts = AtomicUsize::new(0);
        let error = retry_read::<(), _, _>(instant_policy(), || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("401 invalid credentials")
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("401"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
