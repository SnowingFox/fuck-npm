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
//! `Error` whose `code` is [`fnpm_core::Error::code`]; a payload that does not deserialise
//! is `USAGE`, a panic in the core is `CORE_ERROR`. Events are batched by
//! [`fnpm_core::events::BatchingSink`] and delivered through a `ThreadsafeFunction`; the
//! promise settles only after JS has handled the last batch. See
//! `docs/ARCHITECTURE.md#fnpm-napi`.

#![deny(missing_docs)]

mod listener;
mod payload;
#[cfg(feature = "testing")]
mod testing;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use fnpm_core::events::BatchingSink;
use fnpm_core::{CoreRequest, InstallOptions};
use fnpm_types::{EventSink, NoopSink};
use futures_util::FutureExt;
use napi::JsDeferred;
use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use serde_json::Value;

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
    options: Value,
    on_events: Option<EventsFn>,
) -> Result<Object<'env>> {
    let parsed = payload::parse::<InstallOptions>(options, "install options");
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
    request: Value,
    on_events: Option<EventsFn>,
) -> Result<Object<'env>> {
    let parsed = payload::parse::<CoreRequest>(request, "request");
    run(env, parsed, on_events, |request, sink| async move {
        payload::settle(fnpm_core::invoke(request, sink).await)
    })
}

/// Creates the promise and runs `work` on the core runtime with the right sink. A payload
/// that failed to parse rejects right away, without starting the runtime.
fn run<'env, T, W, F>(
    env: &'env Env,
    parsed: std::result::Result<T, Failure>,
    on_events: Option<EventsFn>,
    work: W,
) -> Result<Object<'env>>
where
    T: Send + 'static,
    W: FnOnce(T, Arc<dyn EventSink>) -> F + Send + 'static,
    F: Future<Output = Outcome> + Send + 'static,
{
    let (deferred, promise) = env.create_deferred::<Value, Settle>()?;
    let input = match parsed {
        Ok(input) => input,
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

/// Awaits `work` (a panic becomes `CORE_ERROR`), waits until JS has handled every event
/// batch, then settles the promise.
async fn finish<F>(deferred: JsDeferred<Value, Settle>, listener: Option<Arc<Listener>>, work: F)
where
    F: Future<Output = Outcome>,
{
    let outcome = AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| Err(Failure::from_panic(panic.as_ref())));
    if let Some(listener) = listener {
        listener.close().await;
    }
    deferred.resolve(settle_with(outcome));
}

/// Runs `work` with a batching sink feeding `listener` (flushed every 50 ms and once at
/// the end), or with the no-op sink when nobody listens.
async fn drive<W, F>(listener: Option<Arc<Listener>>, work: W) -> Outcome
where
    W: FnOnce(Arc<dyn EventSink>) -> F,
    F: Future<Output = Outcome>,
{
    let Some(listener) = listener else {
        return work(Arc::new(NoopSink)).await;
    };
    let sink = BatchingSink::new(Arc::new(move |events| {
        listener.deliver(&events);
    }));
    // The guard's drop performs the final flush, before the caller closes the listener.
    let _flusher = sink.start();
    work(sink.clone()).await
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
