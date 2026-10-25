//! The golden fixtures shared with the TS CLI (`tests/fixtures/*.json`, pinned by
//! `packages/fuck-npm/tests/unit/cli/native-fixtures.test.ts`). The TS CLI is the
//! reference; every case here must produce the same bytes natively.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fnpm_cli::color::Colors;
use fnpm_cli::error::CliError;
use fnpm_cli::install::{Core, CoreOutcome};
use fnpm_cli::progress::Renderer;
use fnpm_cli::summary::format_install_summary;
use fnpm_cli::terminal::{Capture, Env, Terminal};
use fnpm_cli::{Bin, run_with};
use fnpm_core::InstallOptions;
use fnpm_core::options::InstallCommand;
use fnpm_types::{Event, EventSink, InstallSummary};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

struct FakeCore {
    events: Vec<Event>,
    summary: InstallSummary,
    error: Option<(String, String)>,
    installs: Mutex<Vec<InstallOptions>>,
}

impl Core for FakeCore {
    fn install(&self, options: InstallOptions, events: Arc<dyn EventSink>) -> CoreOutcome {
        self.installs.lock().unwrap().push(options);
        for event in &self.events {
            events.emit(event.clone());
        }
        match &self.error {
            Some((code, message)) => CoreOutcome::Failed(CliError::core(code, message.clone())),
            None => CoreOutcome::Done(self.summary.clone()),
        }
    }
}

/// Collects failures so one run reports every mismatching case.
#[derive(Default)]
struct Failures(Vec<String>);

impl Failures {
    fn check<T: PartialEq + std::fmt::Debug>(
        &mut self,
        case: &str,
        what: &str,
        actual: &T,
        expected: &T,
    ) {
        if actual != expected {
            self.0.push(format!(
                "{case}: {what}\n  native:   {actual:?}\n  expected: {expected:?}"
            ));
        }
    }

    fn finish(self, total: usize) {
        assert!(
            self.0.is_empty(),
            "{} of {total} cases differ:\n{}",
            self.0.len(),
            self.0.join("\n")
        );
    }
}

#[test]
fn cli_cases_match_the_ts_cli() {
    let data = fixture("cli-cases.json");
    let cwd = PathBuf::from(data["cwd"].as_str().unwrap());
    let default_summary: InstallSummary =
        serde_json::from_value(data["defaultSummary"].clone()).unwrap();
    let cases = data["cases"].as_array().unwrap();
    let mut failures = Failures::default();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let args: Vec<OsString> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().into())
            .collect();
        let env = Env::from_pairs(
            case["env"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned())),
        );
        let stdout = Arc::new(Capture::default());
        let stderr = Arc::new(Capture::default());
        let terminal = Terminal {
            env,
            cwd: cwd.clone(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            stdout_is_tty: case["stdoutIsTTY"].as_bool().unwrap_or(false),
            stderr_is_tty: case["stderrIsTTY"].as_bool().unwrap_or(false),
            columns: case["columns"].as_u64().unwrap_or(120) as usize,
            started: None,
        };
        let core = FakeCore {
            events: serde_json::from_value(
                case.get("events").cloned().unwrap_or(Value::Array(vec![])),
            )
            .unwrap(),
            summary: case
                .get("summary")
                .map(|summary| serde_json::from_value(summary.clone()).unwrap())
                .unwrap_or_else(|| default_summary.clone()),
            error: case.get("coreError").map(|error| {
                (
                    error["code"].as_str().unwrap().to_owned(),
                    error["message"].as_str().unwrap().to_owned(),
                )
            }),
            installs: Mutex::new(Vec::new()),
        };
        let expected = &case["expected"];
        let Some(exit) = run_with(Bin::Fnpm, &args, || terminal, &core) else {
            failures.0.push(format!("{name}: routed to node"));
            continue;
        };
        failures.check(
            name,
            "exit",
            &(exit as i64),
            &expected["exit"].as_i64().unwrap(),
        );
        failures.check(
            name,
            "stdout",
            &stdout.text(),
            &expected["stdout"].as_str().unwrap().to_owned(),
        );
        failures.check(
            name,
            "stderr",
            &stderr.text(),
            &expected["stderr"].as_str().unwrap().to_owned(),
        );
        let installs = core.installs.into_inner().unwrap();
        let expected_install: Option<InstallOptions> = match &expected["install"] {
            Value::Null => None,
            value => Some(serde_json::from_value(value.clone()).unwrap()),
        };
        failures.check(
            name,
            "install options",
            &installs.first(),
            &expected_install.as_ref(),
        );
    }
    failures.finish(cases.len());
}

#[test]
fn summaries_match_the_ts_renderer() {
    let data = fixture("summary.json");
    let cases = data["cases"].as_array().unwrap();
    let mut failures = Failures::default();
    for case in cases {
        let summary: InstallSummary = serde_json::from_value(case["summary"].clone()).unwrap();
        let command: InstallCommand = serde_json::from_value(case["command"].clone()).unwrap();
        let lines = format_install_summary(
            &summary,
            command,
            Colors::new(case["colors"].as_bool().unwrap()),
            case["arrows"].as_bool().unwrap(),
        );
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        failures.check(
            case["name"].as_str().unwrap(),
            "summary",
            &text,
            &case["expected"].as_str().unwrap().to_owned(),
        );
    }
    failures.finish(cases.len());
}

#[test]
fn progress_matches_the_ts_renderer() {
    let data = fixture("progress.json");
    let cases = data["cases"].as_array().unwrap();
    let mut failures = Failures::default();
    for case in cases {
        let mut renderer = Renderer::new(
            Colors::new(case["colors"].as_bool().unwrap()),
            case["columns"].as_u64().unwrap() as usize,
        );
        let mut out = String::new();
        let mut clock = 0.0_f64;
        for step in case["steps"].as_array().unwrap() {
            if let Some(event) = step.get("event") {
                let event: Event = serde_json::from_value(event.clone()).unwrap();
                renderer.event(&event, clock, &mut out);
            } else if let Some(ms) = step.get("advance") {
                clock += ms.as_f64().unwrap();
                if renderer.deadline().is_some_and(|due| due <= clock) {
                    renderer.fire(clock, &mut out);
                }
            } else {
                renderer.stop(&mut out);
            }
        }
        failures.check(
            case["name"].as_str().unwrap(),
            "stderr",
            &out,
            &case["expected"].as_str().unwrap().to_owned(),
        );
    }
    failures.finish(cases.len());
}

/// Differential fuzzing against the TS CLI (`cli-fuzz.json`, generated and pinned by
/// `packages/fuck-npm/tests/unit/cli/native-fuzz.test.ts`): random invocations of the native
/// commands must take the TS planner's route and, when native, produce the same exit code,
/// bytes and `InstallOptions` (stored without the fields equal to their serde defaults).
#[test]
fn fuzzed_invocations_match_the_ts_cli() {
    let data = fixture("cli-fuzz.json");
    let cwd = PathBuf::from(data["cwd"].as_str().unwrap());
    let summary: InstallSummary = serde_json::from_value(data["summary"].clone()).unwrap();
    let terminals = data["terminals"].as_array().unwrap();
    let envs = data["envs"].as_array().unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert!(cases.len() >= 500, "the fuzz corpus is missing cases");
    let mut failures = Failures::default();
    for case in cases {
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned())
            .collect();
        let tty = &terminals[case["terminal"].as_u64().unwrap() as usize];
        let env = &envs[case["env"].as_u64().unwrap() as usize];
        let name = format!("{args:?} tty={tty} env={env}");
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let stdout = Arc::new(Capture::default());
        let stderr = Arc::new(Capture::default());
        let terminal = Terminal {
            env: Env::from_pairs(
                env.as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned())),
            ),
            cwd: cwd.clone(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            stdout_is_tty: tty["stdoutIsTTY"].as_bool().unwrap(),
            stderr_is_tty: tty["stderrIsTTY"].as_bool().unwrap(),
            columns: 120,
            started: None,
        };
        let core = FakeCore {
            events: Vec::new(),
            summary: summary.clone(),
            error: None,
            installs: Mutex::new(Vec::new()),
        };
        let exit = run_with(Bin::Fnpm, &os_args, || terminal, &core);
        let route = case["route"].as_str().unwrap();
        match (exit, route) {
            (None, "node") => continue,
            (None, _) => {
                failures
                    .0
                    .push(format!("{name}: routed to node, the TS CLI runs it"));
                continue;
            }
            (Some(_), "node") => {
                failures.0.push(format!(
                    "{name}: ran natively, the TS CLI routes it to node"
                ));
                continue;
            }
            (Some(_), _) => {}
        }
        let exit = exit.unwrap();
        failures.check(
            &name,
            "exit",
            &i64::from(exit),
            &case["exit"].as_i64().unwrap(),
        );
        failures.check(
            &name,
            "stdout",
            &stdout.text(),
            &case["stdout"].as_str().unwrap().to_owned(),
        );
        failures.check(
            &name,
            "stderr",
            &stderr.text(),
            &case["stderr"].as_str().unwrap().to_owned(),
        );
        let expected: Option<InstallOptions> = match &case["install"] {
            Value::Null => None,
            value => Some(serde_json::from_value(value.clone()).unwrap()),
        };
        let installs = core.installs.into_inner().unwrap();
        failures.check(
            &name,
            "install options",
            &installs.first(),
            &expected.as_ref(),
        );
    }
    failures.finish(cases.len());
}
