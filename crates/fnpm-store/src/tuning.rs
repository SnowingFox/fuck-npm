//! Install tuning knobs read from the environment (bun `bun_core/env_var.rs`):
//!
//! | knob | fnpm | also honoured | default |
//! |---|---|---|---|
//! | bodies at least this large are extracted while downloading | `FNPM_INSTALL_STREAMING_MIN_SIZE` | `BUN_INSTALL_STREAMING_MIN_SIZE` | 2 MiB |
//! | compressed bytes buffered before a streaming drain runs | `FNPM_INSTALL_STREAMING_DRAIN_THRESHOLD` | `BUN_INSTALL_STREAMING_DRAIN_THRESHOLD` | 256 KiB |
//! | Windows rename retry budget | `FNPM_INSTALL_WINDOWS_RENAME_RETRY_MS` | `BUN_INSTALL_WINDOWS_RENAME_RETRY_MS` | 5000 ms |
//! | never stream (always buffer the body) | `FNPM_FEATURE_FLAG_DISABLE_STREAMING_INSTALL` | `BUN_FEATURE_FLAG_DISABLE_STREAMING_INSTALL` | off |
//! | temp dir for staging | `FNPM_TMPDIR` | `BUN_TMPDIR` | `<store>/tmp` |
//!
//! Staging must be on the store's filesystem so the final `rename` is atomic, so the temp dir
//! from the environment is used only when it is on the same device as the store; otherwise
//! the store's own `tmp/` is used.

use std::path::PathBuf;

/// See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tuning {
    /// Bodies at least this large are extracted while downloading.
    pub streaming_min_size: u64,
    /// Compressed bytes buffered before a streaming drain is scheduled.
    pub drain_threshold: usize,
    /// Windows rename retry budget.
    pub windows_rename_retry_ms: u64,
    /// Always buffer whole bodies.
    pub disable_streaming: bool,
    /// Staging directory override (used only on the store's filesystem).
    pub tmpdir: Option<PathBuf>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            streaming_min_size: 2 * 1024 * 1024,
            drain_threshold: 256 * 1024,
            windows_rename_retry_ms: 5000,
            disable_streaming: false,
            tmpdir: None,
        }
    }
}

impl Tuning {
    /// From the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// From any lookup (tests, a captured `fnpm_config::Env`).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| {
            names.iter().find_map(|n| {
                lookup(n)
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            })
        };
        let number = |names: &[&str]| first(names).and_then(|v| v.parse::<u64>().ok());
        let flag = |names: &[&str]| {
            first(names).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        };
        let d = Self::default();
        Self {
            streaming_min_size: number(&[
                "FNPM_INSTALL_STREAMING_MIN_SIZE",
                "BUN_INSTALL_STREAMING_MIN_SIZE",
            ])
            .unwrap_or(d.streaming_min_size),
            drain_threshold: number(&[
                "FNPM_INSTALL_STREAMING_DRAIN_THRESHOLD",
                "BUN_INSTALL_STREAMING_DRAIN_THRESHOLD",
            ])
            .map_or(d.drain_threshold, |n| n.clamp(1, 1 << 30) as usize),
            windows_rename_retry_ms: number(&[
                "FNPM_INSTALL_WINDOWS_RENAME_RETRY_MS",
                "BUN_INSTALL_WINDOWS_RENAME_RETRY_MS",
            ])
            .unwrap_or(d.windows_rename_retry_ms),
            disable_streaming: flag(&[
                "FNPM_FEATURE_FLAG_DISABLE_STREAMING_INSTALL",
                "BUN_FEATURE_FLAG_DISABLE_STREAMING_INSTALL",
            ]),
            tmpdir: first(&["FNPM_TMPDIR", "BUN_TMPDIR"]).map(PathBuf::from),
        }
    }

    /// The body size from which extraction starts before the download ends.
    pub fn effective_streaming_min_size(&self) -> u64 {
        if self.disable_streaming {
            u64::MAX
        } else {
            self.streaming_min_size
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tuning(pairs: &[(&str, &str)]) -> Tuning {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Tuning::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults() {
        let t = tuning(&[]);
        assert_eq!(t, Tuning::default());
        assert_eq!(t.streaming_min_size, 2 << 20);
        assert_eq!(t.drain_threshold, 256 << 10);
        assert_eq!(t.windows_rename_retry_ms, 5000);
    }

    #[test]
    fn fnpm_names_win_over_bun_names() {
        let t = tuning(&[
            ("BUN_INSTALL_STREAMING_MIN_SIZE", "10"),
            ("FNPM_INSTALL_STREAMING_MIN_SIZE", "20"),
            ("BUN_INSTALL_STREAMING_DRAIN_THRESHOLD", "1"),
            ("BUN_TMPDIR", "/tmp/x"),
            ("BUN_FEATURE_FLAG_DISABLE_STREAMING_INSTALL", "1"),
        ]);
        assert_eq!(t.streaming_min_size, 20);
        assert_eq!(t.drain_threshold, 1);
        assert_eq!(t.tmpdir, Some(PathBuf::from("/tmp/x")));
        assert!(t.disable_streaming);
        assert_eq!(t.effective_streaming_min_size(), u64::MAX);
    }

    #[test]
    fn junk_values_fall_back_to_defaults() {
        let t = tuning(&[
            ("FNPM_INSTALL_STREAMING_MIN_SIZE", "lots"),
            ("FNPM_TMPDIR", "  "),
        ]);
        assert_eq!(t.streaming_min_size, 2 << 20);
        assert_eq!(t.tmpdir, None);
    }
}
