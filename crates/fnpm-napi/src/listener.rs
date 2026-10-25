//! Delivery of event batches to the JS `onEvents` callback, with acknowledgement.
//!
//! A batch is handed to Node through a `ThreadsafeFunction` and runs on the JS thread some
//! time later. The promise of `install`/`invoke` settles through a different queue, so
//! without coordination the last progress batch could arrive after the summary. Every batch
//! therefore carries an [`Ack`] that is released once the JS callback has returned, and the
//! promise is only settled after [`Listener::close`] has seen every ack.

use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fnpm_types::Event;
use serde_json::Value;
use tokio::sync::Notify;

/// Where batches go. The production implementation wraps the napi `ThreadsafeFunction`;
/// tests use an in-process fake.
pub trait Transport: Send + Sync {
    /// Queues `batch` for the JS thread. Must arrange for `ack` to be dropped once the
    /// callback has run (or will never run). Returns `false` if the batch was not queued, in
    /// which case `ack` has already been dropped.
    fn send(&self, batch: Value, ack: Ack) -> bool;
}

struct Inner {
    in_flight: AtomicUsize,
    idle: Notify,
}

/// Released when dropped: marks one batch as handled by JS.
pub struct Ack(Arc<Inner>);

impl Drop for Ack {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// The sink-side half: converts core event batches to JSON and sends them, then lets the
/// caller wait until JS has seen all of them.
pub struct Listener {
    transport: Mutex<Option<Box<dyn Transport>>>,
    inner: Arc<Inner>,
}

impl Listener {
    /// Wraps a transport.
    pub fn new(transport: Box<dyn Transport>) -> Arc<Self> {
        Arc::new(Self {
            transport: Mutex::new(Some(transport)),
            inner: Arc::new(Inner {
                in_flight: AtomicUsize::new(0),
                idle: Notify::new(),
            }),
        })
    }

    /// Sends one batch. Empty batches and batches after [`close`](Self::close) are dropped.
    /// Returns whether the batch was queued.
    pub fn deliver(&self, events: &[Event]) -> bool {
        if events.is_empty() {
            return false;
        }
        // `Event` is plain data; serialising it cannot fail.
        let Ok(batch) = serde_json::to_value(events) else {
            return false;
        };
        self.deliver_json(batch)
    }

    /// Sends a batch that is already a JSON array.
    pub fn deliver_json(&self, batch: Value) -> bool {
        let guard = self.transport.lock().unwrap_or_else(|e| e.into_inner());
        let Some(transport) = guard.as_ref() else {
            return false;
        };
        self.inner.in_flight.fetch_add(1, Ordering::AcqRel);
        transport.send(batch, Ack(self.inner.clone()))
    }

    /// Batches sent but not yet acknowledged.
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    /// Waits until JS has handled every batch sent so far, then releases the transport, so
    /// the `ThreadsafeFunction` no longer keeps the event loop alive. Later `deliver` calls
    /// are dropped.
    pub async fn close(&self) {
        loop {
            let mut notified = pin!(self.inner.idle.notified());
            notified.as_mut().enable();
            if self.in_flight() == 0 {
                break;
            }
            notified.await;
        }
        let transport = self
            .transport
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(transport);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    /// Hands every batch to a "JS thread" that sleeps a little before acknowledging.
    struct Slow {
        tx: Mutex<mpsc::Sender<(Value, Ack)>>,
    }

    impl Transport for Slow {
        fn send(&self, batch: Value, ack: Ack) -> bool {
            self.tx.lock().unwrap().send((batch, ack)).is_ok()
        }
    }

    fn slow() -> (Arc<Listener>, thread::JoinHandle<Vec<Value>>) {
        let (tx, rx) = mpsc::channel::<(Value, Ack)>();
        let js = thread::spawn(move || {
            let mut seen = Vec::new();
            for (batch, ack) in rx {
                thread::sleep(Duration::from_millis(5));
                seen.push(batch);
                drop(ack);
            }
            seen
        });
        (Listener::new(Box::new(Slow { tx: Mutex::new(tx) })), js)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn close_waits_for_every_batch_and_releases_the_transport() {
        let (listener, js) = slow();
        for done in 0..5 {
            assert!(listener.deliver(&[Event::Link { done, total: 5 }]));
        }
        runtime().block_on(listener.close());
        assert_eq!(listener.in_flight(), 0);
        // The transport (and its sender) is gone, so the JS thread finishes.
        let seen = js.join().unwrap();
        assert_eq!(seen.len(), 5);
        assert_eq!(seen[4], json!([{ "type": "link", "done": 4, "total": 5 }]));
    }

    #[test]
    fn batches_after_close_and_empty_batches_are_dropped() {
        let (listener, js) = slow();
        assert!(!listener.deliver(&[]));
        runtime().block_on(listener.close());
        assert!(!listener.deliver(&[Event::Link { done: 1, total: 1 }]));
        assert_eq!(listener.in_flight(), 0);
        assert!(js.join().unwrap().is_empty());
    }

    #[test]
    fn close_without_batches_returns_immediately() {
        let (listener, _js) = slow();
        runtime().block_on(listener.close());
    }

    #[test]
    fn a_refused_batch_does_not_block_close() {
        struct Refuse;
        impl Transport for Refuse {
            fn send(&self, _batch: Value, ack: Ack) -> bool {
                drop(ack);
                false
            }
        }
        let listener = Listener::new(Box::new(Refuse));
        assert!(!listener.deliver(&[Event::Link { done: 1, total: 1 }]));
        assert_eq!(listener.in_flight(), 0);
        runtime().block_on(listener.close());
    }

    #[test]
    fn concurrent_producers() {
        let (listener, js) = slow();
        let producers: Vec<_> = (0..4)
            .map(|p| {
                let listener = listener.clone();
                thread::spawn(move || {
                    for i in 0..10 {
                        listener.deliver(&[Event::Warn {
                            message: format!("{p}-{i}"),
                            code: None,
                        }]);
                    }
                })
            })
            .collect();
        for producer in producers {
            producer.join().unwrap();
        }
        runtime().block_on(listener.close());
        assert_eq!(js.join().unwrap().len(), 40);
    }
}
