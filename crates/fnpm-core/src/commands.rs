//! Non-install commands.

use std::sync::Arc;

use fnpm_config::Env;
use fnpm_types::EventSink;

use crate::{CoreRequest, CoreResult, Error, Result};

/// Runs `request.command`.
pub async fn dispatch(
    request: CoreRequest,
    _events: Arc<dyn EventSink>,
    _env: Env,
) -> Result<CoreResult> {
    Err(Error::NotImplemented {
        command: request.command.join(" "),
    })
}
