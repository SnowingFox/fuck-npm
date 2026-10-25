//! `fnpm.lock`: bun.lock's text format (JSONC, trailing commas, 2-space indent). See
//! `docs/ARCHITECTURE.md#fnpmlock` for the schema. Differences from bun.lock: INFO objects
//! carry `deprecated`, and `trustedDependencies` / `patchedDependencies` are sorted.

use fnpm_types::LockGraph;

use crate::text::Dialect;
use crate::{Error, ParseOptions, Result};

/// Parses fnpm.lock text. The edges of the result are bound and re-hoisted exactly like
/// bun binds a loaded bun.lock.
pub fn parse(bytes: &[u8]) -> Result<LockGraph> {
    crate::text_parse(bytes, crate::FNPM_LOCK, &ParseOptions::default()).map(|(g, _)| g)
}

/// Parses fnpm.lock text with options, returning load warnings too.
pub fn parse_with_warnings(bytes: &[u8], opts: &ParseOptions) -> Result<(LockGraph, Vec<String>)> {
    crate::text_parse(bytes, crate::FNPM_LOCK, opts)
}

/// Deterministic: same graph, same bytes. A dependency cycle that cannot be installed is
/// broken at its last nesting (see [`crate::tree::try_hoist`]); [`try_serialize`] and
/// [`crate::save`] report it instead.
pub fn serialize(graph: &LockGraph) -> String {
    crate::text::write::write(graph, Dialect::Fnpm).text
}

/// Like [`serialize`] but fails on an endless nesting cycle, as bun does.
pub fn try_serialize(graph: &LockGraph) -> Result<String> {
    let w = crate::text::write::write(graph, Dialect::Fnpm);
    match w.cycle {
        Some(cycle) => Err(Error::DependencyCycle(cycle)),
        None => Ok(w.text),
    }
}
