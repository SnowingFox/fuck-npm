//! Progress events and the install summary: the JSON the TS CLI renders.
//!
//! The serde shape here is the contract with `packages/fuck-npm/src/core/types.ts`
//! (`CoreEvent`, `InstallSummary`). Rust crates emit through an [`EventSink`]; `fnpm-core`
//! batches them (at most every 50 ms) before they cross into JS.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptPhase {
    Start,
    End,
}

/// `CoreEvent` in `types.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum Event {
    Resolve {
        resolved: u32,
        total: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Download {
        done: u32,
        total: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Extract {
        done: u32,
        total: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Link {
        done: u32,
        total: u32,
    },
    Script {
        package: String,
        script: String,
        phase: ScriptPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Warn {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    Summary {
        summary: InstallSummary,
    },
}

impl Event {
    pub fn warn(message: impl Into<String>, code: Option<&str>) -> Self {
        Self::Warn { message: message.into(), code: code.map(str::to_owned) }
    }

    /// Progress events can be coalesced (only the latest of each kind matters); the others
    /// must all be delivered in order.
    pub fn is_progress(&self) -> bool {
        matches!(self, Self::Resolve { .. } | Self::Download { .. } | Self::Extract { .. } | Self::Link { .. })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRef {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedPackage {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// A newer version that is already in the cache (`(v1.2.4 available)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatedPackage {
    pub name: String,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestedPackage {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_name: Option<String>,
    pub bins: Vec<String>,
    /// Not installed because it does not match the platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedCount {
    pub installs: u32,
    pub packages: u32,
}

/// `InstallSummary` in `types.ts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSummary {
    pub added: Vec<AddedPackage>,
    pub updated: Vec<UpdatedPackage>,
    pub removed: Vec<String>,
    pub requested: Vec<RequestedPackage>,
    pub installed_count: u32,
    pub removed_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<CheckedCount>,
    pub failed_count: u32,
    pub blocked_scripts: u32,
    pub elapsed_ms: u64,
    pub noop: bool,
}

/// Where events go. Implementations must be cheap: they are called from tokio tasks and rayon
/// workers.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);

    /// `false` when nobody listens (`--json`, `--silent`); callers may skip building events.
    fn enabled(&self) -> bool {
        true
    }
}

/// Drops everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: Event) {}
    fn enabled(&self) -> bool {
        false
    }
}

impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    fn emit(&self, event: Event) {
        (**self).emit(event);
    }
    fn enabled(&self) -> bool {
        (**self).enabled()
    }
}

/// Collects events in memory (tests).
#[derive(Debug, Default)]
pub struct VecSink(pub std::sync::Mutex<Vec<Event>>);

impl EventSink for VecSink {
    fn emit(&self, event: Event) {
        self.0.lock().expect("VecSink poisoned").push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_matches_ts_contract() {
        let e = Event::Script { package: "esbuild".into(), script: "postinstall".into(), phase: ScriptPhase::End, exit_code: Some(0) };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"script","package":"esbuild","script":"postinstall","phase":"end","exitCode":0}"#
        );
        let e = Event::Resolve { resolved: 1, total: 2, name: None };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"type":"resolve","resolved":1,"total":2}"#);
        let s = InstallSummary { installed_count: 3, checked: Some(CheckedCount { installs: 1, packages: 2 }), ..Default::default() };
        let j = serde_json::to_value(Event::Summary { summary: s.clone() }).unwrap();
        assert_eq!(j["summary"]["installedCount"], 3);
        assert_eq!(j["summary"]["checked"]["packages"], 2);
        assert_eq!(j["summary"]["noop"], false);
        let back: Event = serde_json::from_value(j).unwrap();
        assert_eq!(back, Event::Summary { summary: s });
        assert!(Event::Link { done: 0, total: 1 }.is_progress());
        assert!(!Event::warn("x", None).is_progress());
        let sink = VecSink::default();
        sink.emit(Event::warn("w", Some("W1")));
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }
}
