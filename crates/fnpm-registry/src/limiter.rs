//! The request limiter: at most `limit` requests in flight, manifests before tarballs, and the
//! adaptive limit of bun's `throttle_after_network_error` (a quarter less, never below 4, once a
//! retried request fails again), which grows back by one after `limit` clean responses.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Request classes; manifests say what else has to be fetched, so they go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    Manifest,
    Tarball,
}

/// The lowest adaptive limit (bun `min_simultaneous_requests`).
const MIN_LIMIT: usize = 4;

/// Failures this close together are one network event and lower the limit once. bun lowers it
/// at most once per `run_tasks` pass, so a burst of streams lost with one connection (an HTTP/2
/// GOAWAY or reset kills every multiplexed request) does not collapse the limit to the minimum.
const THROTTLE_WINDOW: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct State {
    in_flight: usize,
    limit: usize,
    configured: usize,
    clean_since_change: usize,
    last_throttle: Option<Instant>,
    manifests: VecDeque<oneshot::Sender<()>>,
    tarballs: VecDeque<oneshot::Sender<()>>,
}

#[derive(Debug)]
pub(crate) struct Limiter {
    state: Mutex<State>,
}

/// One in-flight slot; released on drop.
#[derive(Debug)]
pub(crate) struct Permit {
    limiter: Arc<Limiter>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.release();
    }
}

/// A queued acquire. If it is dropped after `grant` counted a slot for it, the slot is given back.
struct Waiting<'a> {
    rx: Option<oneshot::Receiver<()>>,
    limiter: &'a Limiter,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(mut rx) = self.rx.take() {
            rx.close();
            if rx.try_recv().is_ok() {
                self.limiter.release();
            }
        }
    }
}

impl Limiter {
    pub fn new(limit: usize) -> Arc<Self> {
        let limit = limit.max(1);
        Arc::new(Self {
            state: Mutex::new(State {
                in_flight: 0,
                limit,
                configured: limit,
                clean_since_change: 0,
                last_throttle: None,
                manifests: VecDeque::new(),
                tarballs: VecDeque::new(),
            }),
        })
    }

    /// Waits for a slot. A tarball only takes a free slot when no manifest is waiting.
    pub async fn acquire(self: &Arc<Self>, priority: Priority) -> Permit {
        let rx = {
            let mut s = self.state.lock();
            let may_take =
                s.in_flight < s.limit && (priority == Priority::Manifest || s.manifests.is_empty());
            if may_take {
                s.in_flight += 1;
                return Permit {
                    limiter: self.clone(),
                };
            }
            let (tx, rx) = oneshot::channel();
            match priority {
                Priority::Manifest => s.manifests.push_back(tx),
                Priority::Tarball => s.tarballs.push_back(tx),
            }
            rx
        };
        let mut waiting = Waiting {
            rx: Some(rx),
            limiter: self,
        };
        // The slot was counted for us by `grant`; the sender is never dropped unsent while
        // `self` is alive.
        let rx = waiting.rx.as_mut().expect("set above");
        let _ = rx.await;
        waiting.rx = None;
        Permit {
            limiter: self.clone(),
        }
    }

    fn release(&self) {
        let mut s = self.state.lock();
        s.in_flight -= 1;
        Self::grant(&mut s);
    }

    /// Hands free slots to waiters, manifests first. A waiter that gave up is skipped.
    fn grant(s: &mut State) {
        while s.in_flight < s.limit {
            let Some(tx) = s.manifests.pop_front().or_else(|| s.tarballs.pop_front()) else {
                return;
            };
            if tx.send(()).is_ok() {
                s.in_flight += 1;
            }
        }
    }

    /// A retried request failed again: lower the limit by a quarter. Returns the new limit if it
    /// changed.
    pub fn throttle(&self) -> Option<usize> {
        self.throttle_at(Instant::now())
    }

    fn throttle_at(&self, now: Instant) -> Option<usize> {
        let mut s = self.state.lock();
        if s.limit <= MIN_LIMIT
            || s.last_throttle
                .is_some_and(|at| now.saturating_duration_since(at) < THROTTLE_WINDOW)
        {
            return None;
        }
        let lowered = MIN_LIMIT.max(s.limit - s.limit / 4);
        if lowered == s.limit {
            return None;
        }
        s.limit = lowered;
        s.clean_since_change = 0;
        s.last_throttle = Some(now);
        Some(lowered)
    }

    /// A response arrived; after `limit` of them in a row the limit grows back by one.
    pub fn record_success(&self) {
        let mut s = self.state.lock();
        if s.limit >= s.configured {
            return;
        }
        s.clean_since_change += 1;
        if s.clean_since_change >= s.limit {
            s.limit += 1;
            s.clean_since_change = 0;
            Self::grant(&mut s);
        }
    }

    pub fn limit(&self) -> usize {
        self.state.lock().limit
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.state.lock().in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn limits_concurrency() {
        let l = Limiter::new(2);
        let a = l.acquire(Priority::Manifest).await;
        let _b = l.acquire(Priority::Tarball).await;
        assert_eq!(l.in_flight(), 2);
        let l2 = l.clone();
        let waiter = tokio::spawn(async move { l2.acquire(Priority::Tarball).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(a);
        let _c = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.in_flight(), 2);
    }

    #[tokio::test]
    async fn manifests_jump_the_queue() {
        let l = Limiter::new(1);
        let held = l.acquire(Priority::Tarball).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (i, p) in [
            Priority::Tarball,
            Priority::Manifest,
            Priority::Tarball,
            Priority::Manifest,
        ]
        .into_iter()
        .enumerate()
        {
            let (l, order) = (l.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let permit = l.acquire(p).await;
                order.lock().push(i);
                tokio::time::sleep(Duration::from_millis(5)).await;
                drop(permit);
            }));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(held);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock(), vec![1, 3, 0, 2]);
        // A free slot is not taken by a tarball while a manifest waits.
        assert_eq!(l.in_flight(), 0);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_leak_a_slot() {
        let l = Limiter::new(1);
        let held = l.acquire(Priority::Manifest).await;
        let l2 = l.clone();
        let waiter = tokio::spawn(async move { l2.acquire(Priority::Manifest).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        waiter.abort();
        let _ = waiter.await;
        drop(held);
        assert_eq!(l.in_flight(), 0);
        let _p = tokio::time::timeout(Duration::from_secs(1), l.acquire(Priority::Tarball))
            .await
            .unwrap();
    }

    #[test]
    fn throttles_by_a_quarter_and_recovers() {
        let t0 = Instant::now();
        let later = |n: u32| t0 + THROTTLE_WINDOW * n;
        let l = Limiter::new(64);
        assert_eq!(l.throttle_at(later(0)), Some(48));
        assert_eq!(l.throttle_at(later(1)), Some(36));
        let l8 = Limiter::new(8);
        assert_eq!(l8.throttle_at(later(0)), Some(6));
        assert_eq!(l8.throttle_at(later(1)), Some(5));
        assert_eq!(l8.throttle_at(later(2)), Some(4));
        assert_eq!(l8.throttle_at(later(3)), None);
        for _ in 0..4 {
            l8.record_success();
        }
        assert_eq!(l8.limit(), 5);
        let small = Limiter::new(3);
        assert_eq!(small.throttle(), None);
    }

    #[test]
    fn a_burst_of_failures_throttles_once() {
        let t0 = Instant::now();
        let l = Limiter::new(64);
        assert_eq!(l.throttle_at(t0), Some(48));
        // 63 more streams of the same lost connection fail within the window.
        for i in 0..63u64 {
            assert_eq!(l.throttle_at(t0 + Duration::from_millis(i)), None);
        }
        assert_eq!(l.limit(), 48);
        assert_eq!(l.throttle_at(t0 + THROTTLE_WINDOW), Some(36));
    }
}
