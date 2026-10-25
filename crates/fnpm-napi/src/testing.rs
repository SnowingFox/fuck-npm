//! Test-only exports (cargo feature `testing`, never in release builds). They drive the
//! binding's own plumbing (event delivery, acknowledgement, rejection codes, panic
//! handling) through the same code paths as `install`/`invoke`, without depending on what
//! the core does. `__testDeliver` hands ready-made batches to the listener on a private
//! runtime; `__testEmit` goes through `run` exactly like `install`, batching sink included.

// `#[napi]` items are only referenced by the generated registration code, which is not
// compiled into unit-test builds.
#![cfg_attr(test, allow(dead_code))]

use std::sync::OnceLock;
use std::time::Duration;

use fnpm_types::Event;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use serde_json::Value;

use crate::listener::Listener;
use crate::payload::{self, Failure};
use crate::{EventsFn, Settle, TsfnTransport, finish, run, settle_with};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

/// Delivers each element of `batches` (each a JSON array of events) through `onEvents` from
/// a runtime thread, then resolves with
/// `{ delivered }` once JS has handled all of them. With `fail = { code, message }` it
/// rejects instead; with `panic = "msg"` the work panics.
#[napi(js_name = "__testDeliver")]
pub fn test_deliver<'env>(
    env: &'env Env,
    batches: Vec<Value>,
    on_events: Option<EventsFn>,
    fail: Option<Value>,
    panic: Option<String>,
) -> Result<Object<'env>> {
    let (deferred, promise) = env.create_deferred::<Value, Settle>()?;
    let listener = on_events.map(|tsfn| Listener::new(Box::new(TsfnTransport(tsfn))));
    let producer = listener.clone();
    let work = async move {
        let mut delivered = 0_u32;
        if let Some(listener) = &producer {
            for batch in batches {
                if listener.deliver_json(batch) {
                    delivered += 1;
                }
                tokio::task::yield_now().await;
            }
        }
        if let Some(message) = panic {
            panic!("{message}");
        }
        match fail {
            Some(fail) => Err(Failure {
                code: leak_code(
                    fail.get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("CORE_ERROR"),
                ),
                message: fail
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            }),
            None => Ok(serde_json::json!({ "delivered": delivered })),
        }
    };
    runtime().spawn(finish(deferred, listener, work));
    Ok(promise)
}

/// Emits `events` (each a `CoreEvent` object) one by one into the sink the work gets from
/// `run`, sleeping `pauseMs` after every `pauseEvery` events, then resolves with
/// `{ emitted }`. Progress events coalesce and batches follow the 50 ms flush, as in an
/// install.
#[napi(js_name = "__testEmit")]
pub fn test_emit<'env>(
    env: &'env Env,
    events: Value,
    on_events: Option<Unknown<'env>>,
    pause_every: Option<u32>,
    pause_ms: Option<u32>,
) -> Result<Object<'env>> {
    let parsed = payload::parse::<Vec<Event>>(events, "events");
    let pause_every = pause_every.unwrap_or(0) as usize;
    let pause = Duration::from_millis(u64::from(pause_ms.unwrap_or(0)));
    run(env, parsed, on_events, move |events, sink| async move {
        let emitted = events.len();
        for (i, event) in events.into_iter().enumerate() {
            sink.emit(event);
            if pause_every > 0 && (i + 1) % pause_every == 0 {
                tokio::time::sleep(pause).await;
            }
        }
        Ok(serde_json::json!({ "emitted": emitted }))
    })
}

/// Settles synchronously with a failure, as a payload error does.
#[napi(js_name = "__testRejectNow")]
pub fn test_reject_now<'env>(
    env: &'env Env,
    code: String,
    message: String,
) -> Result<Object<'env>> {
    let (deferred, promise) = env.create_deferred::<Value, Settle>()?;
    deferred.resolve(settle_with(Err(Failure {
        code: leak_code(&code),
        message,
    })));
    Ok(promise)
}

/// `Failure::code` is `&'static str` because real codes are constants; tests pass a handful
/// of strings, so leaking them is fine.
fn leak_code(code: &str) -> &'static str {
    Box::leak(code.to_owned().into_boxed_str())
}
