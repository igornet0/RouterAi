//! Retry policy for transient failures.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AiError;

/// Exponential / linear retry configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Max attempts including the first try.
    pub max_attempts: u32,
    /// Initial backoff (milliseconds in serde).
    #[serde(with = "duration_millis")]
    pub initial_delay: Duration,
    /// Cap on backoff (milliseconds in serde).
    #[serde(with = "duration_millis")]
    pub max_delay: Duration,
    /// Use exponential backoff.
    pub exponential_backoff: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            exponential_backoff: true,
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt `n` (0-based after first failure).
    pub fn delay_for_attempt(&self, attempt: u32, err: &AiError) -> Duration {
        if let Some(secs) = err.retry_after_secs() {
            return Duration::from_secs(secs).min(self.max_delay);
        }
        if !self.exponential_backoff {
            return self.initial_delay.min(self.max_delay);
        }
        let factor = 2u32.saturating_pow(attempt);
        self.initial_delay
            .saturating_mul(factor)
            .min(self.max_delay)
    }

    /// Should we retry this error given attempts so far (1 = first try done).
    /// A `Retry-After` longer than `max_delay` is not retried early: the request
    /// moves on to the fallback policy instead.
    pub fn should_retry(&self, attempts: u32, err: &AiError) -> bool {
        attempts < self.max_attempts
            && err.is_retryable()
            && err
                .retry_after_secs()
                .map_or(true, |s| Duration::from_secs(s) <= self.max_delay)
    }
}

/// Run an async operation with retry.
pub async fn with_retry<F, Fut, T>(policy: &RetryPolicy, mut op: F) -> Result<T, AiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AiError>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if !policy.should_retry(attempt, &err) {
                    return Err(err);
                }
                let delay = policy.delay_for_attempt(attempt.saturating_sub(1), &err);
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %err,
                    "retrying AI request"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(d: &Duration, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retries_rate_limit_then_succeeds() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            exponential_backoff: false,
        };
        let count = AtomicU32::new(0);
        let result = with_retry(&policy, || async {
            let n = count.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(AiError::RateLimit {
                    provider: None,
                    retry_after_secs: None,
                    message: "wait".into(),
                })
            } else {
                Ok(42)
            }
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn long_retry_after_is_not_retried_early() {
        let policy = RetryPolicy::default();
        let rl = |secs| AiError::RateLimit {
            provider: None,
            retry_after_secs: Some(secs),
            message: "wait".into(),
        };
        assert!(policy.should_retry(1, &rl(1)));
        assert!(!policy.should_retry(1, &rl(3600)));
    }

    #[tokio::test]
    async fn does_not_retry_auth() {
        let policy = RetryPolicy::default();
        let err = with_retry(&policy, || async {
            Err::<(), _>(AiError::Authentication {
                provider: None,
                message: "bad key".into(),
            })
        })
        .await
        .unwrap_err();
        assert!(matches!(err, AiError::Authentication { .. }));
    }
}
