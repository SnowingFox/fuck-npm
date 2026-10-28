//! Batching between Rust producers and the JS callback.
//!
//! Producers (tokio tasks, rayon workers, link threads) call [`EventSink::emit`] at any rate;
//! the consumer (the napi `ThreadsafeFunction`, or the native renderer) receives at most one
//! batch every [`FLUSH_INTERVAL`]. Progress events of one kind coalesce to the latest value
//! (keeping the position of the first one in the batch); every other event is delivered once,
//! in order.

use std::io::IsTerminal as _;
use std::sync::Arc;
use std::time::Duration;

use fnpm_types::{Event, EventSink};
use parking_lot::Mutex;

/// The interval the TS renderer expects.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// The five progress kinds, as indices into [`Pending::progress_slot`].
fn progress_kind(event: &Event) -> Option<usize> {
    match event {
        Event::Resolve { .. } => Some(0),
        Event::Download { .. } => Some(1),
        Event::Extract { .. } => Some(2),
        Event::Link { .. } => Some(3),
        Event::Telemetry { .. } => Some(4),
        _ => None,
    }
}

#[derive(Default)]
struct Pending {
    events: Vec<Event>,
    /// Where the pending progress event of each kind sits in `events`.
    progress_slot: [Option<usize>; 5],
}

/// The deliver callback: receives one non-empty batch.
pub type Deliver = Arc<dyn Fn(Vec<Event>) + Send + Sync>;

/// Collects events from any thread. Progress events of the same kind coalesce to the latest;
/// everything else is queued in order. A tokio task calls `flush` every [`FLUSH_INTERVAL`]
/// and once at the end, handing the batch to `deliver` (the napi ThreadsafeFunction).
pub struct BatchingSink {
    deliver: Deliver,
    wants_progress: bool,
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
            wants_progress: live_progress(
                std::io::stderr().is_terminal(),
                &fnpm_config::Env::from_process(),
            ),
            pending: Mutex::new(Pending::default()),
            delivering: Mutex::new(()),
        })
    }

    /// Delivers pending events now. Returns how many were sent.
    pub fn flush(&self) -> usize {
        let _order = self.delivering.lock();
        let batch = {
            let mut p = self.pending.lock();
            p.progress_slot = [None; 5];
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

/// Mirror the native/TS renderer gate once per sink, keeping per-package checks cheap.
fn live_progress(stderr_is_tty: bool, env: &fnpm_config::Env) -> bool {
    stderr_is_tty
        && env
            .get("FNPM_PROGRESS")
            .or_else(|| env.get("BUN_INSTALL_PROGRESS"))
            != Some("0")
        && !env.is_ci()
}

impl EventSink for BatchingSink {
    fn wants_progress(&self) -> bool {
        self.wants_progress
    }

    fn emit(&self, event: Event) {
        // The TS renderer prints warnings and errors as they come: mask URL credentials here.
        let event = match event {
            Event::Warn { message, code } => Event::Warn {
                message: fnpm_config::redact::redacted_message(&message).into_owned(),
                code,
            },
            Event::Error { message, code } => Event::Error {
                message: fnpm_config::redact::redacted_message(&message).into_owned(),
                code,
            },
            other => other,
        };
        // Hand inherited-output boundaries to JS before the runner acknowledges a start.
        let foreground = matches!(
            &event,
            Event::Script {
                foreground: true,
                ..
            } | Event::Output { .. }
        );
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
        drop(p);
        if foreground {
            self.flush();
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

/// Registry sampling exists only for a live progress consumer. The timer keeps a weak
/// reference, so cancellation cannot retain the client after the guard is dropped.
pub(crate) struct TelemetryGuard {
    state: Arc<Telemetry>,
    task: tokio::task::JoinHandle<()>,
}

struct Telemetry {
    client: fnpm_registry::RegistryClient,
    sink: Arc<dyn EventSink>,
    cpus: u32,
    io_threads: u32,
    reused_packages: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes timer emissions, link-setting updates and the final snapshot.
    settings: Mutex<TelemetrySettings>,
}

#[derive(Default)]
struct TelemetrySettings {
    link_threads: u32,
    stopped: bool,
}

impl Telemetry {
    fn emit(&self, settings: &TelemetrySettings) {
        let stats = self.client.stats();
        self.sink.emit(Event::Telemetry {
            cpus: self.cpus,
            io_threads: self.io_threads,
            link_threads: settings.link_threads,
            network_active: self.client.active_requests().min(u32::MAX as usize) as u32,
            network_limit: self.client.concurrency_limit().min(u32::MAX as usize) as u32,
            bytes: stats.bytes_downloaded,
            requests: stats
                .manifest_requests
                .saturating_add(stats.tarball_requests),
            cache_hits: stats.manifest_cache_hits.saturating_add(stats.not_modified),
            retries: stats.retries,
            reused_packages: self
                .reused_packages
                .load(std::sync::atomic::Ordering::Relaxed),
        });
    }

    fn sample(&self) {
        let settings = self.settings.lock();
        if !settings.stopped {
            self.emit(&settings);
        }
    }
}

impl TelemetryGuard {
    pub(crate) fn start(
        client: &fnpm_registry::RegistryClient,
        sink: &Arc<dyn EventSink>,
        progress: bool,
    ) -> Option<Self> {
        if !progress || !sink.enabled() || !sink.wants_progress() {
            return None;
        }
        let state = Arc::new(Telemetry {
            client: client.clone(),
            sink: sink.clone(),
            cpus: fnpm_types::available_parallelism().min(u32::MAX as usize) as u32,
            io_threads: crate::io_threads().min(u32::MAX as usize) as u32,
            reused_packages: Arc::default(),
            settings: Mutex::default(),
        });
        state.sample();
        let weak = Arc::downgrade(&state);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(state) = weak.upgrade() else { break };
                state.sample();
            }
        });
        Some(Self { state, task })
    }

    pub(crate) fn reuse_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.state.reused_packages.clone()
    }

    /// The pool size actually selected for this linking pass, rather than a CPU estimate.
    pub(crate) fn link_threads(&self, threads: usize) {
        let mut settings = self.state.settings.lock();
        settings.link_threads = threads.min(u32::MAX as usize) as u32;
        if !settings.stopped {
            self.state.emit(&settings);
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.task.abort();
        let mut settings = self.state.settings.lock();
        settings.stopped = true;
        self.state.emit(&settings);
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
        sink.emit(Event::Link {
            done: 1,
            total: 2,
            name: None,
        });
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
                Event::Link {
                    done: 1,
                    total: 2,
                    name: None
                },
                Event::warn("second", Some("W")),
            ]]
        );
        // Nothing pending: no empty batch.
        assert_eq!(sink.flush(), 0);
        assert_eq!(batches.lock().len(), 1);
        // A new batch starts a new coalescing window.
        sink.emit(Event::Link {
            done: 2,
            total: 2,
            name: None,
        });
        assert_eq!(sink.flush(), 1);
    }

    #[test]
    fn batch_sampling_gate_matches_live_renderer_environment() {
        let empty = fnpm_config::Env::default();
        assert!(!live_progress(false, &empty));
        assert!(live_progress(true, &empty));
        for (key, value) in [
            ("FNPM_PROGRESS", "0"),
            ("BUN_INSTALL_PROGRESS", "0"),
            ("CI", "1"),
            ("GITHUB_ACTIONS", "true"),
        ] {
            let env = fnpm_config::Env::from_pairs([(key, value)]);
            assert!(!live_progress(true, &env), "{key}={value}");
        }
        let env = fnpm_config::Env::from_pairs([
            ("FNPM_PROGRESS", "1"),
            ("BUN_INSTALL_PROGRESS", "0"),
            ("CI", "false"),
        ]);
        assert!(
            live_progress(true, &env),
            "fnpm setting takes precedence over bun fallback"
        );
    }

    #[test]
    fn foreground_boundaries_flush_before_emit_returns() {
        let (sink, batches) = collecting();
        sink.emit(Event::ScriptPlan { total: 1 });
        for phase in [
            fnpm_types::event::ScriptPhase::Start,
            fnpm_types::event::ScriptPhase::End,
        ] {
            sink.emit(Event::Script {
                package: "root".into(),
                script: "install".into(),
                phase,
                id: Some("0:/root:install".into()),
                foreground: true,
                exit_code: None,
            });
            let batches = batches.lock();
            assert!(matches!(batches.last().unwrap().last().unwrap(),
                Event::Script { phase: delivered, .. } if *delivered == phase));
        }
        assert_eq!(batches.lock().len(), 2);
        assert_eq!(sink.flush(), 0);
    }

    #[tokio::test]
    async fn telemetry_is_opt_in_periodic_and_finishes_without_retaining_client() {
        let dir = tempfile::tempdir().unwrap();
        let config = Arc::new(fnpm_config::Config::defaults(dir.path()));
        let client =
            fnpm_registry::RegistryClient::with_env(config, fnpm_config::Env::default()).unwrap();
        let passive: Arc<dyn EventSink> = Arc::new(fnpm_types::event::VecSink::default());
        assert!(TelemetryGuard::start(&client, &passive, true).is_none());
        let events = Arc::new(Mutex::new(Vec::new()));
        struct Live(Arc<Mutex<Vec<Event>>>);
        impl EventSink for Live {
            fn emit(&self, event: Event) {
                self.0.lock().push(event);
            }
            fn wants_progress(&self) -> bool {
                true
            }
        }
        let sink: Arc<dyn EventSink> = Arc::new(Live(events.clone()));
        assert!(TelemetryGuard::start(&client, &sink, false).is_none());
        assert!(events.lock().is_empty());
        let guard = TelemetryGuard::start(&client, &sink, true).unwrap();
        let weak = Arc::downgrade(&guard.state);
        assert_eq!(events.lock().len(), 1, "immediate snapshot");
        assert!(matches!(events.lock()[0], Event::Telemetry {
            cpus, io_threads, link_threads: 0, network_active: 0, network_limit,
            bytes: 0, requests: 0, cache_hits: 0, retries: 0, reused_packages: 0
        } if cpus > 0 && io_threads > 0 && network_limit > 0));
        guard.reuse_counter().fetch_add(7, Ordering::Relaxed);
        guard.link_threads(3);
        assert!(matches!(
            events.lock().last().unwrap(),
            Event::Telemetry {
                link_threads: 3,
                reused_packages: 7,
                ..
            }
        ));
        tokio::time::sleep(Duration::from_millis(550)).await;
        assert!(events.lock().len() >= 3, "500ms snapshot");
        let before = events.lock().len();
        drop(guard);
        assert_eq!(events.lock().len(), before + 1, "final snapshot");
        assert!(
            weak.upgrade().is_none(),
            "timer must not retain its registry client"
        );
        tokio::time::sleep(Duration::from_millis(550)).await;
        assert_eq!(events.lock().len(), before + 1, "sampling stopped");
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
