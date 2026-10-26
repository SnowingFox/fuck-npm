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
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Resource settings and cumulative registry counters for the live renderer.
    Telemetry {
        cpus: u32,
        io_threads: u32,
        link_threads: u32,
        network_active: u32,
        network_limit: u32,
        bytes: u64,
        requests: u64,
        cache_hits: u64,
        retries: u64,
        /// Packages read from the local content store, separate from manifest cache hits.
        #[serde(default, skip_serializing_if = "is_zero")]
        reused_packages: u64,
    },
    /// Scripts scheduled by one runner invocation; successive plans are additive.
    ScriptPlan {
        total: u32,
    },
    Script {
        package: String,
        script: String,
        phase: ScriptPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The script inherits output, so live renderers must suspend drawing until its end.
        #[serde(default, skip_serializing_if = "is_false")]
        foreground: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    Output {
        active: bool,
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
    /// A line bun prints on stderr when it shows no progress bar: `Resolving dependencies`,
    /// `Resolved, downloaded and extracted [N]`, `Saved lockfile`. A progress renderer
    /// ignores it.
    Status {
        message: String,
    },
    /// A line every renderer prints, above a progress region too (bun's
    /// `migrated lockfile from package-lock.json`).
    Notice {
        message: String,
    },
    Summary {
        summary: InstallSummary,
    },
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Event {
    pub fn status(message: impl Into<String>) -> Self {
        Self::Status {
            message: message.into(),
        }
    }

    pub fn notice(message: impl Into<String>) -> Self {
        Self::Notice {
            message: message.into(),
        }
    }

    pub fn warn(message: impl Into<String>, code: Option<&str>) -> Self {
        Self::Warn {
            message: message.into(),
            code: code.map(str::to_owned),
        }
    }

    /// Progress events can be coalesced (only the latest of each kind matters); the others
    /// must all be delivered in order.
    pub fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::Resolve { .. }
                | Self::Download { .. }
                | Self::Extract { .. }
                | Self::Link { .. }
                | Self::Telemetry { .. }
        )
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

    /// Opts into periodic resource sampling when progress is also enabled in the options.
    fn wants_progress(&self) -> bool {
        false
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
    fn wants_progress(&self) -> bool {
        (**self).wants_progress()
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
    fn notice_shape() {
        assert_eq!(
            serde_json::to_string(&Event::notice("migrated lockfile from bun.lock")).unwrap(),
            r#"{"type":"notice","message":"migrated lockfile from bun.lock"}"#
        );
    }

    #[test]
    fn telemetry_and_script_contract_are_backward_compatible() {
        let telemetry = Event::Telemetry {
            cpus: 8,
            io_threads: 4,
            link_threads: 3,
            network_active: 2,
            network_limit: 48,
            bytes: 1024,
            requests: 10,
            cache_hits: 6,
            retries: 1,
            reused_packages: 0,
        };
        let json = serde_json::json!({
            "type": "telemetry", "cpus": 8, "ioThreads": 4, "linkThreads": 3,
            "networkActive": 2, "networkLimit": 48,
            "bytes": 1024, "requests": 10, "cacheHits": 6, "retries": 1,
        });
        assert_eq!(serde_json::to_value(&telemetry).unwrap(), json);
        assert_eq!(serde_json::from_value::<Event>(json).unwrap(), telemetry);
        assert!(telemetry.is_progress());
        assert_eq!(
            serde_json::to_value(Event::ScriptPlan { total: 7 }).unwrap(),
            serde_json::json!({"type": "scriptplan", "total": 7})
        );
        assert!(!Event::ScriptPlan { total: 7 }.is_progress());
        let legacy = serde_json::json!({
            "type": "script", "package": "pkg", "script": "install", "phase": "start"
        });
        let event = serde_json::from_value::<Event>(legacy.clone()).unwrap();
        assert!(matches!(
            &event,
            Event::Script {
                id: None,
                foreground: false,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(event).unwrap(), legacy);
        let foreground = serde_json::json!({
            "type": "script", "package": "pkg", "script": "install", "phase": "start",
            "id": "0:/pkg:install", "foreground": true
        });
        let event = serde_json::from_value::<Event>(foreground.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), foreground);
        let link = serde_json::json!({"type": "link", "done": 0, "total": 2});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Event>(link.clone()).unwrap()).unwrap(),
            link
        );
    }

    #[test]
    fn progress_sampling_requires_explicit_opt_in_and_arc_delegates() {
        struct Progress;
        impl EventSink for Progress {
            fn emit(&self, _: Event) {}
            fn wants_progress(&self) -> bool {
                true
            }
        }
        assert!(!NoopSink.wants_progress());
        assert!(!VecSink::default().wants_progress());
        let sink: Arc<dyn EventSink> = Arc::new(Progress);
        assert!(sink.wants_progress());
        assert!(Arc::new(sink).wants_progress());
    }

    #[test]
    fn json_matches_ts_contract() {
        let e = Event::Script {
            package: "esbuild".into(),
            script: "postinstall".into(),
            phase: ScriptPhase::End,
            id: None,
            foreground: false,
            exit_code: Some(0),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"script","package":"esbuild","script":"postinstall","phase":"end","exitCode":0}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::status("Saved lockfile")).unwrap(),
            r#"{"type":"status","message":"Saved lockfile"}"#
        );
        let e = Event::Resolve {
            resolved: 1,
            total: 2,
            name: None,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"resolve","resolved":1,"total":2}"#
        );
        let s = InstallSummary {
            installed_count: 3,
            checked: Some(CheckedCount {
                installs: 1,
                packages: 2,
            }),
            ..Default::default()
        };
        let j = serde_json::to_value(Event::Summary { summary: s.clone() }).unwrap();
        assert_eq!(j["summary"]["installedCount"], 3);
        assert_eq!(j["summary"]["checked"]["packages"], 2);
        assert_eq!(j["summary"]["noop"], false);
        let back: Event = serde_json::from_value(j).unwrap();
        assert_eq!(back, Event::Summary { summary: s });
        assert!(
            Event::Link {
                done: 0,
                total: 1,
                name: None
            }
            .is_progress()
        );
        assert!(!Event::warn("x", None).is_progress());
        let sink = VecSink::default();
        sink.emit(Event::warn("w", Some("W1")));
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }
}
