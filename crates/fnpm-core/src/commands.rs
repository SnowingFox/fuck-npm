//! Non-install commands. `request.command` is the path the CLI registry
//! (`packages/fuck-npm/src/cli/trees/*.ts`) defines: `outdated`, `why`, `ls`, `info`, `link`,
//! `unlink`, `patch`, `patch-commit`, `prune`, `dedupe`, `audit`, `pack`, `publish`, `run`,
//! `exec`, `x`, `create`, `init`, `cache …`, `pkg …`, `pm …`, `store path|status|prune`,
//! `doctor`, `whoami`, `migrate`, and `lock …` for `fnpm-lock`. The table in
//! ARCHITECTURE.md#fnpm-core maps each to its bun source.

use std::sync::Arc;

use fnpm_types::EventSink;

use crate::{CoreRequest, CoreResult, Error, Result};

pub async fn dispatch(request: CoreRequest, _events: Arc<dyn EventSink>) -> Result<CoreResult> {
    let path: Vec<&str> = request.command.iter().map(String::as_str).collect();
    match path.as_slice() {
        ["outdated"]
        | ["why"]
        | ["ls"]
        | ["info"]
        | ["link"]
        | ["unlink"]
        | ["patch"]
        | ["patch-commit"]
        | ["prune"]
        | ["dedupe"]
        | ["audit", ..]
        | ["pack"]
        | ["publish"]
        | ["run"]
        | ["exec"]
        | ["x"]
        | ["create"]
        | ["init"]
        | ["cache", ..]
        | ["pkg", ..]
        | ["pm", ..]
        | ["store", ..]
        | ["doctor"]
        | ["whoami"]
        | ["migrate"]
        | ["lock", ..] => todo!("command {:?}", request.command),
        _ => Err(Error::UnknownCommand(request.command)),
    }
}
