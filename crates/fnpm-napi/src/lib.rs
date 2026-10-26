//! Node-API bindings for the fnpm core, loaded by `@fuck-npm/binding`. The JS surface is
//! `packages/fuck-npm-binding/index.d.ts`:
//!
//! ```ts
//! version(): string
//! install(options: InstallOptions, onEvents?: (err, events: CoreEvent[]) => void): Promise<InstallSummary>
//! invoke(request: CoreRequest, onEvents?: (err, events: CoreEvent[]) => void): Promise<CoreResult>
//! ```
//!
//! Payloads cross as JSON values (serde, camelCase, unknown fields ignored). The work runs on
//! [`fnpm_core::runtime`], never on the JS thread. Failures reject the promise with an
//! `Error` whose `code` is [`fnpm_core::Error::code`]; a payload or callback that does not
//! match the contract is `USAGE`, a panic in the core is `CORE_ERROR`. Neither function
//! throws synchronously. Events are batched by [`fnpm_core::events::BatchingSink`] and
//! delivered through a `ThreadsafeFunction`; the promise settles only after JS has handled
//! the last batch. See `docs/ARCHITECTURE.md#fnpm-napi`.

#![deny(missing_docs)]

mod listener;
mod payload;
#[cfg(feature = "testing")]
mod testing;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use fnpm_core::events::{BatchingSink, FLUSH_INTERVAL};
use fnpm_core::{CoreRequest, InstallOptions};
use fnpm_types::{Event, EventSink, NoopSink};
use futures_util::FutureExt;
use napi::JsDeferred;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::listener::{Ack, Listener, Transport};
use crate::payload::Failure;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `onEvents`, called as `(null, events)` on the JS thread. Its return value is ignored; if
/// it throws, the exception is raised as an uncaught exception, like an event listener's.
type EventsFn = ThreadsafeFunction<Value, Unknown<'static>, Value, Status, true>;

/// The outcome of the work: the resolved value or the rejection.
type Outcome = std::result::Result<Value, Failure>;

/// What a settled promise is built from on the JS thread.
type Settle = Box<dyn FnOnce(Env) -> Result<Value> + Send>;

/// The version of the native core. The CLI warns when it differs from its own.
#[napi]
pub fn version() -> String {
    fnpm_core::version().to_owned()
}

/// The install pipeline (`install`, `ci`, `add`, `remove`, `update`). `options` is
/// `InstallOptions`; resolves with `InstallSummary`. `onEvents` receives batched
/// `CoreEvent[]` (at most every 50 ms); when it is absent the core uses a no-op sink and
/// builds no events.
#[napi(
    ts_args_type = "options: Record<string, unknown>, onEvents?: (err: Error | null, events: Array<{ type: string; [key: string]: unknown }>) => void",
    ts_return_type = "Promise<Record<string, unknown>>"
)]
pub fn install<'env>(
    env: &'env Env,
    options: Unknown<'env>,
    on_events: Option<Unknown<'env>>,
) -> Result<Object<'env>> {
    const WHAT: &str = "install options";
    let parsed = payload::from_js::<InstallOptions>(env, options, WHAT)
        .and_then(|options| payload::absolute_cwd(&options.pm.cwd, WHAT).map(|()| options));
    run(env, parsed, on_events, |options, sink| async move {
        payload::settle(fnpm_core::install(options, sink).await)
    })
}

/// Every other command. `request` is `CoreRequest` (`{command: ["pm", "cache", "rm"],
/// cwd, positionals, passthrough, options, json}`); resolves with `CoreResult`.
#[napi(
    ts_args_type = "request: Record<string, unknown>, onEvents?: (err: Error | null, events: Array<{ type: string; [key: string]: unknown }>) => void",
    ts_return_type = "Promise<{ exitCode: number; data?: Record<string, unknown>; lines?: string[] }>"
)]
pub fn invoke<'env>(
    env: &'env Env,
    request: Unknown<'env>,
    on_events: Option<Unknown<'env>>,
) -> Result<Object<'env>> {
    const WHAT: &str = "request";
    let parsed = payload::from_js::<CoreRequest>(env, request, WHAT)
        .and_then(|request| payload::absolute_cwd(&request.cwd, WHAT).map(|()| request));
    run(env, parsed, on_events, |request, sink| async move {
        payload::settle(fnpm_core::invoke(request, sink).await)
    })
}

/// Node marks its stdio descriptors close-on-exec, so a child that the core spawns with
/// inherited stdio (`fnpm run`, `fnpm x`, lifecycle scripts) would start with fds 0-2
/// closed and fail its first write with `EBADF`. Clearing the flag only affects processes
/// spawned without explicit stdio; Node's own `child_process` always sets them up itself.
fn inherit_stdio() {
    #[cfg(unix)]
    for fd in 0..=2 {
        // SAFETY: the descriptor is only borrowed for the two `fcntl` calls; a closed one
        // makes them fail with EBADF, which is ignored.
        let fd = unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) };
        if let Ok(flags) = rustix::io::fcntl_getfd(fd)
            && flags.contains(rustix::io::FdFlags::CLOEXEC)
        {
            let _ = rustix::io::fcntl_setfd(fd, flags - rustix::io::FdFlags::CLOEXEC);
        }
    }
}

/// Creates the promise and runs `work` on the core runtime with the right sink. A payload
/// or callback that does not match the contract rejects right away, without starting the
/// runtime or creating the `ThreadsafeFunction`.
fn run<'env, T, W, F>(
    env: &'env Env,
    parsed: std::result::Result<T, Failure>,
    on_events: Option<Unknown<'env>>,
    work: W,
) -> Result<Object<'env>>
where
    T: Send + 'static,
    W: FnOnce(T, Arc<dyn EventSink>) -> F + Send + 'static,
    F: Future<Output = Outcome> + Send + 'static,
{
    inherit_stdio();
    let (deferred, promise) = env.create_deferred::<Value, Settle>()?;
    let prepared = payload::callback(on_events)
        .and_then(|callback| parsed.map(|input| (input, callback)))
        .and_then(|(input, callback)| Ok((input, events_fn(env, callback)?)));
    let (input, on_events) = match prepared {
        Ok(prepared) => prepared,
        Err(failure) => {
            deferred.resolve(settle_with(Err(failure)));
            return Ok(promise);
        }
    };
    let runtime = match std::panic::catch_unwind(fnpm_core::runtime) {
        Ok(runtime) => runtime,
        Err(panic) => {
            deferred.resolve(settle_with(Err(Failure::from_panic(panic.as_ref()))));
            return Ok(promise);
        }
    };
    let listener = on_events.map(|tsfn| Listener::new(Box::new(TsfnTransport(tsfn))));
    let work = drive(listener.clone(), move |sink| work(input, sink));
    runtime.spawn(finish(deferred, listener, work));
    Ok(promise)
}

/// Turns a validated `onEvents` function into the `ThreadsafeFunction` that delivers batches.
fn events_fn(
    env: &Env,
    callback: Option<Unknown<'_>>,
) -> std::result::Result<Option<EventsFn>, Failure> {
    let Some(callback) = callback else {
        return Ok(None);
    };
    // SAFETY: `callback` is a live function value of this call's `env`.
    unsafe { EventsFn::from_napi_value(env.raw(), callback.raw()) }
        .map(Some)
        .map_err(|error| Failure::usage(format!("invalid onEvents: {}", error.reason)))
}

/// Awaits `work`, waits until JS has handled every event batch, then settles the promise.
async fn finish<F>(deferred: JsDeferred<Value, Settle>, listener: Option<Arc<Listener>>, work: F)
where
    F: Future<Output = Outcome>,
{
    let outcome = guarded(work).await;
    if let Some(listener) = listener {
        listener.close().await;
    }
    deferred.resolve(settle_with(outcome));
}

/// Runs `work`, turning a panic into `CORE_ERROR`.
async fn guarded<F>(work: F) -> Outcome
where
    F: Future<Output = Outcome>,
{
    AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| Err(Failure::from_panic(panic.as_ref())))
}

/// Runs `work` with a batching sink feeding `listener` (flushed every 50 ms and once at
/// the end), or with the no-op sink when nobody listens. Must run inside a tokio runtime.
async fn drive<W, F>(listener: Option<Arc<Listener>>, work: W) -> Outcome
where
    W: FnOnce(Arc<dyn EventSink>) -> F,
    F: Future<Output = Outcome>,
{
    let Some(listener) = listener else {
        return guarded(work(Arc::new(NoopSink))).await;
    };
    let sink = BatchingSink::new(Arc::new(move |events: Vec<Event>| {
        if events.iter().any(|event| {
            matches!(
                event,
                Event::Script {
                    foreground: true,
                    ..
                } | Event::Output { .. }
            )
        }) {
            listener.deliver_before_output(&events);
        } else {
            listener.deliver(&events);
        }
    }));
    let flusher = Flusher::start(sink.clone());
    let outcome = guarded(work(sink)).await;
    // The final flush happens before the caller closes the listener.
    flusher.finish().await;
    outcome
}

/// The periodic flush of a [`BatchingSink`].
///
/// `BatchingSink::start` is not used: its guard aborts the timer task and flushes right
/// away, while the task may be in the middle of a flush on another worker (batch taken,
/// not yet delivered). The final batch could then overtake the previous one, or the
/// previous one could arrive after the listener closed and be lost. Here the final flush
/// waits until the timer task has actually stopped.
struct Flusher {
    sink: Arc<BatchingSink>,
    task: Option<JoinHandle<()>>,
}

impl Flusher {
    fn start(sink: Arc<BatchingSink>) -> Self {
        let ticking = sink.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The first tick completes immediately.
            tick.tick().await;
            loop {
                tick.tick().await;
                ticking.flush();
            }
        });
        Self {
            sink,
            task: Some(task),
        }
    }

    /// Stops the timer, waits out a flush it may be running, then delivers what is left.
    async fn finish(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // Resolves once the task is gone: `flush` has no await point, so a flush that
            // was running has delivered its batch by then.
            let _ = task.await;
        }
        self.sink.flush();
    }
}

impl Drop for Flusher {
    /// The drive future was dropped before `finish` (runtime shutdown): stop the timer.
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// The settle callback for an outcome: the value, or an `Error` with `code` set.
fn settle_with(outcome: Outcome) -> Settle {
    Box::new(move |env| match outcome {
        Ok(value) => Ok(value),
        Err(failure) => Err(js_error(&env, &failure)?),
    })
}

/// Builds `Object.assign(new Error(message), { code })` and wraps it so the promise rejects
/// with exactly that object.
fn js_error(env: &Env, failure: &Failure) -> Result<Error> {
    let mut error =
        env.create_error(Error::new(Status::GenericFailure, failure.message.clone()))?;
    error.set_named_property("code", failure.code)?;
    Ok(Error::from_unknown_without_coercion(error.to_unknown()))
}

/// Sends batches through the `ThreadsafeFunction`, non-blocking (its queue is unbounded).
/// The ack rides in the return-value callback, which runs after the JS callback returned.
struct TsfnTransport(EventsFn);

impl Transport for TsfnTransport {
    fn send(&self, batch: Value, ack: Ack) -> bool {
        // If Node refuses the call (the env is closing), napi-rs leaks the queued closure,
        // so keep the ack where it can be released here instead.
        let slot = Arc::new(Mutex::new(Some(ack)));
        let in_callback = slot.clone();
        let status = self.0.call_with_return_value(
            Ok(batch),
            ThreadsafeFunctionCallMode::NonBlocking,
            move |returned, _env| {
                drop(in_callback.lock().unwrap_or_else(|e| e.into_inner()).take());
                returned.map(drop)
            },
        );
        if status == Status::Ok {
            return true;
        }
        drop(slot.lock().unwrap_or_else(|e| e.into_inner()).take());
        false
    }
}
