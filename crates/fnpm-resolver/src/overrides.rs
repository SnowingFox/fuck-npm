//! Override matching (bun `OverrideMap.rs`): which [`OverrideRule`] applies to an edge.

use fnpm_types::{OverrideRule, Package};

/// Parses the root package.json `overrides` (npm, nested objects one level deep) or, if
/// absent, `resolutions` (yarn), plus pnpm-workspace overrides. `$name` references are
/// resolved against the root's declared specs. Returns the rules and warnings.
pub fn parse_overrides(_root: &serde_json::Value) -> (Vec<OverrideRule>, Vec<String>) {
    todo!()
}

/// The rule for dependency `name` declared as `declared` by `parent` (None for the root).
/// Precedence: ranged parent > parent > flat; +1 for a matching target range; ties by parent
/// range text, then target range text.
pub fn find<'a>(
    _rules: &'a [OverrideRule],
    _parent: Option<&Package>,
    _name: &str,
    _declared: &str,
) -> Option<&'a OverrideRule> {
    todo!()
}
