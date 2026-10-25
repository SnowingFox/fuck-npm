//! Batching between Rust producers and the JS callback.

use std::sync::Arc;
use std::time::Duration;

use fnpm_types::{Event, EventSink};

/// The interval the TS renderer expects.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Collects events from any thread. Progress events of the same kind coalesce to the latest;
/// everything else is queued in order. A tokio task calls `flush` every [`FLUSH_INTERVAL`]
/// and once at the end, handing the batch to `deliver` (the napi ThreadsafeFunction).
pub struct BatchingSink {
    #[allow(dead_code)]
    deliver: Arc<dyn Fn(Vec<Event>) + Send + Sync>,
}

impl BatchingSink {
    pub fn new(deliver: Arc<dyn Fn(Vec<Event>) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { deliver })
    }

    /// Delivers pending events now. Returns how many were sent.
    pub fn flush(&self) -> usize {
        todo!()
    }

    /// Spawns the periodic flusher on the current tokio runtime; it stops when the returned
    /// guard is dropped (after a final flush).
    pub fn start(self: &Arc<Self>) -> FlushGuard {
        todo!()
    }
}

impl EventSink for BatchingSink {
    fn emit(&self, _event: Event) {
        todo!()
    }
}

pub struct FlushGuard {}
