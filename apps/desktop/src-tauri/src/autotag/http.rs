//! What the HTTPS providers share: the client, and how an HTTP failure maps onto the plan's
//! failure classes.

use reqwest::header::{HeaderMap, RETRY_AFTER};
use std::time::Duration;

use super::provider::ErrorClass;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// An HTTPS client with the providers' timeouts, or why it couldn't be set up.
pub fn client() -> Result<reqwest::Client, String> {
    // reqwest's rustls needs a process-wide provider; installing it twice is harmless
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("Could not set up HTTPS: {e}"))
}

/// The plan's failure class for an HTTP error status.
pub fn classify_status(status: u16) -> ErrorClass {
    match status {
        408 | 429 | 500..=599 => ErrorClass::Retryable,
        401..=404 => ErrorClass::Fatal,
        _ => ErrorClass::Item,
    }
}

/// `retry-after` in seconds.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // Negative, NaN, infinite or too long for a Duration: ignored like any other garbage
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn http_statuses_map_to_the_plan_failure_classes() {
        for status in [408, 429, 500, 502, 503, 529] {
            assert_eq!(classify_status(status), ErrorClass::Retryable, "{status}");
        }
        for status in [401, 402, 403, 404] {
            assert_eq!(classify_status(status), ErrorClass::Fatal, "{status}");
        }
        for status in [400, 413, 415, 422] {
            assert_eq!(classify_status(status), ErrorClass::Item, "{status}");
        }
    }

    #[test]
    fn retry_after_is_read_in_seconds() {
        let headers = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
            map
        };

        assert_eq!(retry_after(&headers("30")), Some(Duration::from_secs(30)));
        assert_eq!(
            retry_after(&headers("1.5")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(retry_after(&headers("soon")), None);
        assert_eq!(retry_after(&headers("-1")), None);
        assert_eq!(
            retry_after(&headers("1e20")),
            None,
            "too long for a Duration"
        );
        assert_eq!(retry_after(&headers("NaN")), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
