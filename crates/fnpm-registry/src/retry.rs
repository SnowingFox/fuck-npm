//! Retry policy (bun `install/retry_timer.rs` and `runTasks.rs::download_outcome`).
//!
//! A request is retried when no response arrived (connect error, reset, closed connection) or
//! the status is 5xx, 429 or 408, up to `network.retries` times. Not retried: certificate
//! errors and redirect loops (the next attempt fails the same way), and for a request that a
//! caller blocks on, an idle timeout or a 501. The wait before retry `n` (1-based) is
//! `base << min(n-1, 6)`, capped at `base * 40`, ±20% jitter; a larger `Retry-After: <seconds>`
//! (at most 60) replaces it. A base of 0 retries immediately.

use std::time::Duration;

/// What one attempt ended with, as far as retrying is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A response with this status arrived.
    Status(u16),
    /// No response: connection refused/reset/closed, DNS failure, or the body broke off.
    NoResponse,
    /// The idle timeout elapsed.
    Timeout,
    /// TLS certificate rejected, too many redirects, or another error that repeats.
    Final,
}

/// Whether to try again (`blocking`: the user waits on this one request).
pub(crate) fn should_retry(outcome: Outcome, blocking: bool) -> bool {
    match outcome {
        Outcome::Final => false,
        Outcome::Timeout => !blocking,
        Outcome::NoResponse => true,
        Outcome::Status(501) if blocking => false,
        Outcome::Status(s) => s > 499 || s == 429 || s == 408,
    }
}

/// `+-20%`, so a fleet of installs does not retry in step.
pub(crate) fn backoff(base_ms: u64, attempt: u16, random: u64) -> Duration {
    if base_ms == 0 {
        return Duration::ZERO;
    }
    let shift = u32::from(attempt.saturating_sub(1).min(6));
    let ms = (base_ms << shift).min(base_ms.saturating_mul(40));
    let jitter = ms / 5;
    Duration::from_millis(ms - jitter + random % (2 * jitter + 1))
}

/// `Retry-After: <seconds>`, at most a minute. The HTTP-date form is ignored.
pub(crate) fn retry_after(value: Option<&str>) -> Option<Duration> {
    let value = value?.trim_matches([' ', '\t']);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs: u64 = value.parse().unwrap_or(u64::MAX);
    Some(Duration::from_secs(secs.min(60)))
}

/// The wait before retry `attempt` and whether the registry asked for it.
pub(crate) fn wait_before(
    base_ms: u64,
    attempt: u16,
    retry_after_header: Option<&str>,
    random: u64,
) -> (Duration, bool) {
    if base_ms == 0 {
        return (Duration::ZERO, false);
    }
    let wait = backoff(base_ms, attempt, random);
    match retry_after(retry_after_header).filter(|asked| *asked > wait) {
        Some(asked) => (asked, true),
        None => (wait, false),
    }
}

/// A cheap per-process random number for jitter (xorshift over the clock and a counter).
pub(crate) fn fast_random() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = STATE.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_outcomes() {
        for s in [408, 429, 500, 502, 503, 599] {
            assert!(should_retry(Outcome::Status(s), false), "{s}");
        }
        for s in [200, 304, 400, 401, 403, 404] {
            assert!(!should_retry(Outcome::Status(s), false), "{s}");
        }
        assert!(should_retry(Outcome::Status(501), false));
        assert!(!should_retry(Outcome::Status(501), true));
        assert!(should_retry(Outcome::NoResponse, true));
        assert!(should_retry(Outcome::Timeout, false));
        assert!(!should_retry(Outcome::Timeout, true));
        assert!(!should_retry(Outcome::Final, false));
    }

    #[test]
    fn exponential_backoff_with_jitter() {
        // Without jitter (random = jitter), the waits are 50, 100, 200, ... capped at 2000.
        let mid = |base: u64, attempt: u16| {
            let ms = (base << attempt.saturating_sub(1).min(6)).min(base * 40);
            backoff(base, attempt, ms / 5).as_millis() as u64
        };
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7, 8].map(|a| mid(50, a)),
            [50, 100, 200, 400, 800, 1600, 2000, 2000]
        );
        for attempt in 1..10 {
            for r in [0, 7, 1 << 40, u64::MAX] {
                let ms = backoff(50, attempt, r).as_millis() as u64;
                let nominal = (50u64 << (attempt - 1).min(6)).min(2000);
                assert!(
                    ms >= nominal - nominal / 5 && ms <= nominal + nominal / 5,
                    "{attempt} {ms}"
                );
            }
        }
        assert_eq!(backoff(1, 1, 12345), Duration::from_millis(1));
        assert_eq!(backoff(0, 3, 1), Duration::ZERO);
    }

    #[test]
    fn retry_after_header() {
        assert_eq!(retry_after(Some("1")), Some(Duration::from_secs(1)));
        assert_eq!(retry_after(Some(" 30\t")), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(Some("86400")), Some(Duration::from_secs(60)));
        for bad in ["", "-1", "soon", "Wed, 21 Oct 2096 07:28:00 GMT", "1.5"] {
            assert_eq!(retry_after(Some(bad)), None, "{bad}");
        }
        assert_eq!(
            wait_before(1, 1, Some("1"), 0),
            (Duration::from_secs(1), true)
        );
        assert_eq!(
            wait_before(1, 1, Some("0"), 0),
            (Duration::from_millis(1), false)
        );
        assert_eq!(wait_before(0, 1, Some("3600"), 0), (Duration::ZERO, false));
        assert_eq!(
            wait_before(1, 1, None, 0),
            (Duration::from_millis(1), false)
        );
    }
}
