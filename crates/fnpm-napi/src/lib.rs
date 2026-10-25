//! Node-API bindings for the fnpm core. The JS surface is exactly
//! `packages/fuck-npm-binding/index.d.ts`:
//!
//! ```ts
//! version(): string
//! install(options: InstallOptions, onEvents?: (err, events: CoreEvent[]) => void): Promise<InstallSummary>
//! invoke(request: CoreRequest, onEvents?: (err, events: CoreEvent[]) => void): Promise<CoreResult>
//! ```
//!
//! Payloads cross as JSON values (serde). Errors reject the promise with `code` set to
//! `fnpm_core::Error::code()`. See `docs/ARCHITECTURE.md#fnpm-napi`.

use napi::bindgen_prelude::*;
use napi::threadsafe_function::ThreadsafeFunction;
use napi_derive::napi;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[napi]
pub fn version() -> String {
    fnpm_core::version().to_owned()
}

/// `onEvents` receives batched `CoreEvent[]` (at most every 50 ms). When it is absent the
/// core uses a no-op sink and builds no events.
#[napi]
pub async fn install(
    options: serde_json::Value,
    on_events: Option<ThreadsafeFunction<serde_json::Value>>,
) -> Result<serde_json::Value> {
    let _ = (options, on_events);
    Err(Error::from_reason(
        "fnpm-napi: install is not implemented yet",
    ))
}

#[napi]
pub async fn invoke(
    request: serde_json::Value,
    on_events: Option<ThreadsafeFunction<serde_json::Value>>,
) -> Result<serde_json::Value> {
    let _ = (request, on_events);
    Err(Error::from_reason(
        "fnpm-napi: invoke is not implemented yet",
    ))
}
