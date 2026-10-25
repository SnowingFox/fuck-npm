//! Batching between Rust producers and the JS callback.
//!
//! Producers (tokio tasks, rayon workers, link threads) call [`EventSink::emit`] at any rate;
//! the consumer (the napi `ThreadsafeFunction`, or the native renderer) receives at most one
//! batch every [`FLUSH_INTERVAL`]. Progress events of one kind coalesce to the latest value
//! (keeping the position of the first one in the batch); every other event is delivered once,
//! in order.

use std::sync::Arc;
use std::time::Duration;

use fnpm_types::{Event, EventSink};
use parking_lot::Mutex;

/// The interval the TS renderer expects.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// The four progress kinds, as indices into [`Pending::progress_slot`].
fn progress_kind(event: &Event) -> Option<usize> {
    match event {
        Event::Resolve { .. } => Some(0),
        Event::Download { .. } => Some(1),
        Event::Extract { .. } => Some(2),
        Event::Link { .. } => Some(3),
        _ => None,
    }
}

#[derive(Default)]
struct Pending {
    events: Vec<Event>,
    /// Where the pending progress event of each kind sits in `events`.
    progress_slot: [Option<usize>; 4],
}

/// The deliver callback: receives one non-empty batch.
pub type Deliver = Arc<dyn Fn(Vec<Event>) + Send + Sync>;

/// Collects events from any thread. Progress events of the same kind coalesce to the latest;
/// everything else is queued in order. A tokio task calls `flush` every [`FLUSH_INTERVAL`]
/// and once at the end, handing the batch to `deliver` (the napi ThreadsafeFunction).
pub struct BatchingSink {
    deliver: Deliver,
    pending: Mutex<Pending>,
    /// Held while a batch is taken and delivered, so the flusher task and the final flush
    /// never hand two batches over out of order.
    delivering: Mutex<()>,
}

impl BatchingSink {
    /// A sink that hands batches to `deliver`. Nothing is delivered until [`BatchingSink::flush`]
    /// or the flusher started by [`BatchingSink::start`] runs.
    pub fn new(deliver: Deliver) -> Arc<Self> {
        Arc::new(Self {
            deliver,
            pending: Mutex::new(Pending::default()),
            delivering: Mutex::new(()),
        })
    }

    /// Delivers pending events now. Returns how many were sent.
    pub fn flush(&self) -> usize {
        let _order = self.delivering.lock();
        let batch = {
            let mut p = self.pending.lock();
            p.progress_slot = [None; 4];
            std::mem::take(&mut p.events)
        };
        let n = batch.len();
        if n > 0 {
            (self.deliver)(batch);
        }
        n
    }

    /// Spawns the periodic flusher on the current tokio runtime (or [`crate::runtime`] when
    /// called outside one); it stops when the returned guard is dropped, after a final flush.
    pub fn start(self: &Arc<Self>) -> FlushGuard {
        let sink = self.clone();
        let task = async move {
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                sink.flush();
            }
        };
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h.spawn(task),
            Err(_) => crate::runtime().spawn(task),
        };
        FlushGuard {
            sink: self.clone(),
            task: Some(handle),
        }
    }
}

impl EventSink for BatchingSink {
    fn emit(&self, event: Event) {
        let mut p = self.pending.lock();
        match progress_kind(&event) {
            Some(kind) => match p.progress_slot[kind] {
                Some(at) => p.events[at] = event,
                None => {
                    let at = p.events.len();
                    p.events.push(event);
                    p.progress_slot[kind] = Some(at);
                }
            },
            None => p.events.push(event),
        }
    }
}

/// Stops the periodic flusher of a [`BatchingSink`] when dropped, then delivers whatever is
/// still pending.
pub struct FlushGuard {
    sink: Arc<BatchingSink>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl FlushGuard {
    /// Stops the flusher and delivers the last batch now (same as dropping the guard).
    pub fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            self.sink.flush();
        }
    }
}

impl Drop for FlushGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Batches = Arc<Mutex<Vec<Vec<Event>>>>;

    fn collecting() -> (Arc<BatchingSink>, Batches) {
        let batches: Batches = Arc::default();
        let b = batches.clone();
        (
            BatchingSink::new(Arc::new(move |e| b.lock().push(e))),
            batches,
        )
    }

    #[test]
    fn progress_coalesces_and_others_keep_order() {
        let (sink, batches) = collecting();
        sink.emit(Event::Resolve {
            resolved: 1,
            total: 10,
            name: None,
        });
        sink.emit(Event::warn("first", None));
        sink.emit(Event::Resolve {
            resolved: 5,
            total: 10,
            name: None,
        });
        sink.emit(Event::Link { done: 1, total: 2 });
        sink.emit(Event::warn("second", Some("W")));
        assert_eq!(sink.flush(), 4);
        let got = batches.lock().clone();
        assert_eq!(
            got,
            vec![vec![
                Event::Resolve {
                    resolved: 5,
                    total: 10,
                    name: None
                },
                Event::warn("first", None),
                Event::Link { done: 1, total: 2 },
                Event::warn("second", Some("W")),
            ]]
        );
        // Nothing pending: no empty batch.
        assert_eq!(sink.flush(), 0);
        assert_eq!(batches.lock().len(), 1);
        // A new batch starts a new coalescing window.
        sink.emit(Event::Link { done: 2, total: 2 });
        assert_eq!(sink.flush(), 1);
    }

    #[test]
    fn concurrent_emitters_lose_nothing() {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let sink = BatchingSink::new(Arc::new(move |e| {
            c.fetch_add(e.len(), Ordering::SeqCst);
        }));
        std::thread::scope(|s| {
            for t in 0..8 {
                let sink = &sink;
                s.spawn(move || {
                    for i in 0..1000 {
                        sink.emit(Event::warn(format!("{t}-{i}"), None));
                        if i % 100 == 0 {
                            sink.flush();
                        }
                    }
                });
            }
        });
        sink.flush();
        assert_eq!(count.load(Ordering::SeqCst), 8000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn periodic_flush_and_final_flush_on_drop() {
        let (sink, batches) = collecting();
        let guard = sink.start();
        sink.emit(Event::warn("a", None));
        tokio::time::sleep(FLUSH_INTERVAL * 3).await;
        assert_eq!(batches.lock().len(), 1, "flushed by the timer");
        sink.emit(Event::warn("b", None));
        drop(guard);
        let got = batches.lock().clone();
        assert_eq!(got.len(), 2, "final flush on drop");
        assert_eq!(got[1], vec![Event::warn("b", None)]);
    }
}
