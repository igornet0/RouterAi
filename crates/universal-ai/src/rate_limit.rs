//! Rate limit state extracted from response headers.

use chrono::{DateTime, TimeZone, Utc};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

/// Observed rate limit window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Remaining requests.
    pub requests_remaining: Option<u64>,
    /// Request limit.
    pub requests_limit: Option<u64>,
    /// Remaining tokens.
    pub tokens_remaining: Option<u64>,
    /// Token limit.
    pub tokens_limit: Option<u64>,
    /// Reset timestamp.
    pub reset_at: Option<DateTime<Utc>>,
}

impl RateLimit {
    /// Parse common OpenAI-style headers.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            requests_remaining: header_u64(headers, "x-ratelimit-remaining-requests"),
            requests_limit: header_u64(headers, "x-ratelimit-limit-requests"),
            tokens_remaining: header_u64(headers, "x-ratelimit-remaining-tokens"),
            tokens_limit: header_u64(headers, "x-ratelimit-limit-tokens"),
            reset_at: header_reset(headers),
        }
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn header_reset(headers: &HeaderMap) -> Option<DateTime<Utc>> {
    // Prefer epoch seconds if present; otherwise leave unset.
    if let Some(secs) = header_u64(headers, "x-ratelimit-reset-requests") {
        return Utc.timestamp_opt(secs as i64, 0).single();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn parses_openai_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("10"),
        );
        h.insert("x-ratelimit-limit-requests", HeaderValue::from_static("60"));
        let rl = RateLimit::from_headers(&h);
        assert_eq!(rl.requests_remaining, Some(10));
        assert_eq!(rl.requests_limit, Some(60));
    }
}
