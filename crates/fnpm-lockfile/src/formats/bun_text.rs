//! `bun.lock` (text lockfileVersion 0-3, bun `lockfile/bun.lock.rs`): the same reader and
//! writer as fnpm.lock, with bun's exact output (no `deprecated`, bun's hash-map order for
//! `trustedDependencies` and `patchedDependencies`), so `fnpm-lock export --to bun` gives
//! the file bun would write.

use fnpm_types::LockGraph;

use crate::text::Dialect;
use crate::{ParseOptions, Result};

/// Parses bun.lock text.
pub fn parse(bytes: &[u8]) -> Result<LockGraph> {
    crate::text_parse(bytes, "bun.lock", &ParseOptions::default()).map(|(g, _)| g)
}

/// Parses bun.lock text and returns the warnings bun prints while loading it (an
/// unsupported integrity in a lockfileVersion 0/1 file is ignored with a warning).
pub fn parse_with_warnings(bytes: &[u8], opts: &ParseOptions) -> Result<(LockGraph, Vec<String>)> {
    crate::text_parse(bytes, "bun.lock", opts)
}

/// Writes bun.lock text, byte-identical to what bun writes for the same lockfile.
pub fn serialize(graph: &LockGraph) -> String {
    crate::text::write::write(graph, Dialect::Bun).text
}
