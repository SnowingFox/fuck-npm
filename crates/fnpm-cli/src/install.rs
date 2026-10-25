//! `install`, `ci`, `add`, `remove` and `update`, run in process.
//!
//! The order of business is the TS CLI's (`runParsedCli` then `runInstallFamily`): unknown
//! flags, flag parsing, required positionals, routing (`install <pkgs>` / `install -g` are
//! `add`, `ci` is a frozen install, `update -i` goes to Node), option validation, header,
//! the core, and the summary. Output and exit codes match byte for byte.

use std::sync::Arc;

use fnpm_core::InstallOptions;
use fnpm_core::options::InstallCommand;
use fnpm_types::{Event, EventSink, InstallSummary, NoopSink};

use crate::color::Colors;
use crate::error::{CliError, ErrorKind};
use crate::options::install_options;
use crate::parse::{parse_flags, reject_unknown_flags};
use crate::plan::Invocation;
use crate::progress::{DiagnosticsSink, ProgressSink, wants_progress};
use crate::spec::{BinDef, LeafDef, VERSION};
use crate::summary::format_install_summary;
use crate::terminal::{Terminal, color_enabled};

/// What the core did with an install.
#[derive(Debug)]
pub enum CoreOutcome {
    Done(InstallSummary),
    Failed(CliError),
    /// SIGINT / Ctrl-C arrived; the pipeline was dropped.
    Interrupted,
}

/// The install pipeline as the launcher sees it. [`NativeCore`] runs `fnpm_core::install`;
/// tests substitute a scripted one.
pub trait Core {
    /// Run one install with `events` receiving progress and diagnostics.
    fn install(&self, options: InstallOptions, events: Arc<dyn EventSink>) -> CoreOutcome;
}

/// `fnpm_core::install` on the core's runtime, cancelled by Ctrl-C.
///
/// The synchronous part of the pipeline (`fnpm_core::prepare_install`: options, project,
/// configuration, the no-op fast path) runs first, without a runtime, so a no-op install is
/// answered before the runtime exists: building the multi-thread tokio runtime (cgroup
/// probing, worker threads, the signal driver) costs more than the whole no-op check
/// (`docs/ARCHITECTURE.md#startup-budget`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeCore;

impl Core for NativeCore {
    fn install(&self, options: InstallOptions, events: Arc<dyn EventSink>) -> CoreOutcome {
        let failed = |error: fnpm_core::Error| {
            CoreOutcome::Failed(CliError::core(error.code(), error.to_string()))
        };
        let prepared = match fnpm_core::prepare_install(options, fnpm_config::Env::from_process()) {
            Ok(prepared) => prepared,
            Err(error) => return failed(error),
        };
        if let Some(summary) = prepared.noop() {
            // What `PreparedInstall::run` does for a no-op, without a runtime.
            for warning in prepared.warnings() {
                events.emit(Event::warn(warning.clone(), None));
            }
            return CoreOutcome::Done(summary.clone());
        }
        fnpm_core::runtime().block_on(async move {
            let interrupted = async {
                // Without a signal driver there is nothing to wait for; never resolve.
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                result = prepared.run(events) => match result {
                    Ok(summary) => CoreOutcome::Done(summary),
                    Err(error) => failed(error),
                },
                () = interrupted => CoreOutcome::Interrupted,
            }
        })
    }
}

/// How a native command ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit with this status.
    Exit(i32),
    /// The command turned out to need the TS CLI (`update -i`); exec it with the argv.
    Node,
}

/// Exit status for an interrupted install (128 + SIGINT).
pub const EXIT_INTERRUPTED: i32 = 130;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Json,
    Rich,
    Plain,
}

/// Run a planned native invocation.
pub fn run(invocation: &Invocation, bin: &BinDef, terminal: &Terminal, core: &dyn Core) -> Outcome {
    let leaf = invocation.leaf;
    let prepared = match prepare(invocation, bin, terminal) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => return Outcome::Node,
        Err(error) => {
            return Outcome::Exit(report_error(
                terminal,
                invocation.wants_json,
                bin,
                leaf,
                &error,
            ));
        }
    };
    let (options, label) = prepared;

    let mode = if invocation.wants_json {
        Mode::Json
    } else if terminal.stdout_is_tty {
        Mode::Rich
    } else {
        Mode::Plain
    };
    let silent = options.pm.log_level == fnpm_config::LogLevel::Silent;
    let stderr_colors = Colors::new(color_enabled(&terminal.env, terminal.stderr_is_tty));

    // `fnpm add v0.1.0` and a blank line on stderr, like bun's header.
    if mode != Mode::Json && !silent && options.pm.summary {
        terminal.stderr.write(&format!(
            "{} {}\n\n",
            stderr_colors.bold(&format!("fnpm {label}")),
            stderr_colors.dim(&format!("v{VERSION}"))
        ));
    }

    // Who hears core events: nobody under --json / --silent (the core may skip building
    // them), the progress line on a terminal, diagnostics otherwise.
    let mut progress: Option<Arc<ProgressSink>> = None;
    let sink: Arc<dyn EventSink> = if mode == Mode::Json || silent {
        Arc::new(NoopSink)
    } else if mode == Mode::Rich
        && wants_progress(
            &terminal.env,
            terminal.stderr_is_tty,
            false,
            options.pm.log_level == fnpm_config::LogLevel::Quiet,
            !options.pm.progress,
        )
    {
        let live = ProgressSink::start(
            stderr_colors,
            terminal.columns,
            Arc::clone(&terminal.stderr),
        );
        progress = Some(Arc::clone(&live));
        live
    } else {
        Arc::new(DiagnosticsSink::new(
            stderr_colors,
            Arc::clone(&terminal.stderr),
        ))
    };

    let command = options.command;
    let show_summary = !silent && options.pm.summary;
    let outcome = core.install(options, sink);
    if let Some(progress) = progress {
        progress.stop();
    }

    match outcome {
        CoreOutcome::Interrupted => Outcome::Exit(EXIT_INTERRUPTED),
        CoreOutcome::Failed(error) => Outcome::Exit(report_error(
            terminal,
            invocation.wants_json,
            bin,
            leaf,
            &error,
        )),
        CoreOutcome::Done(summary) => {
            let exit_code = i32::from(summary.failed_count > 0);
            if mode == Mode::Json {
                let payload = serde_json::json!({
                    "ok": exit_code == 0,
                    "command": command_name(command),
                    "summary": summary,
                });
                terminal.stdout.write(&format!("{payload}\n"));
                return Outcome::Exit(exit_code);
            }
            if show_summary {
                let stdout_color = color_enabled(&terminal.env, terminal.stdout_is_tty);
                let lines = format_install_summary(
                    &summary,
                    command,
                    Colors::new(stdout_color),
                    mode == Mode::Rich && stdout_color,
                );
                let mut text = String::new();
                for line in lines {
                    text.push_str(&line);
                    text.push('\n');
                }
                terminal.stdout.write(&text);
            }
            Outcome::Exit(exit_code)
        }
    }
}

/// `InstallOptions.command` as the TS CLI serialises it.
pub fn command_name(command: InstallCommand) -> &'static str {
    match command {
        InstallCommand::Install => "install",
        InstallCommand::Add => "add",
        InstallCommand::Remove => "remove",
        InstallCommand::Update => "update",
    }
}

/// Validate everything and build the options; `Ok(None)` means "hand this to Node".
/// Also returns the header label (`ci` keeps its own name).
fn prepare(
    invocation: &Invocation,
    bin: &BinDef,
    terminal: &Terminal,
) -> Result<Option<(InstallOptions, &'static str)>, CliError> {
    let leaf = invocation.leaf;
    reject_unknown_flags(&invocation.own, leaf, bin)?;
    let parsed = parse_flags(&invocation.own, leaf)?;
    let mut positionals = parsed.positionals.clone();
    positionals.extend(invocation.forwarded.iter().cloned());
    require_positionals(bin, leaf, &positionals)?;

    let mut command = match leaf.name {
        "add" => InstallCommand::Add,
        "remove" => InstallCommand::Remove,
        "update" => InstallCommand::Update,
        _ => InstallCommand::Install,
    };
    if leaf.name == "install" && (parsed.bool("global") || !positionals.is_empty()) {
        command = InstallCommand::Add;
    }
    if command == InstallCommand::Update && parsed.bool("interactive") {
        return Ok(None);
    }
    let options = install_options(
        command,
        &parsed,
        &positionals,
        &terminal.cwd,
        leaf.name == "ci",
    )?;
    let label = if leaf.name == "ci" {
        "ci"
    } else {
        command_name(command)
    };
    Ok(Some((options, label)))
}

/// `requirePositionals`: the first missing required positional, with its description.
fn require_positionals(
    bin: &BinDef,
    leaf: &LeafDef,
    positionals: &[String],
) -> Result<(), CliError> {
    let required: Vec<_> = leaf
        .positionals
        .iter()
        .filter(|positional| positional.required)
        .collect();
    if positionals.len() >= required.len() {
        return Ok(());
    }
    let missing = required[positionals.len()];
    Err(CliError::missing(format!(
        "`{} {}` needs <{}>: {}.",
        bin.name, leaf.name, missing.name, missing.description
    )))
}

/// `reportError`: JSON on stdout under `--json`, otherwise `error: …` on stderr plus, for a
/// usage error, the help hint. Always exit 1.
pub fn report_error(
    terminal: &Terminal,
    wants_json: bool,
    bin: &BinDef,
    leaf: &LeafDef,
    error: &CliError,
) -> i32 {
    if wants_json {
        let payload = serde_json::json!({ "ok": false, "error": { "code": error.code, "message": error.message } });
        terminal.stdout.write(&format!("{payload}\n"));
        return 1;
    }
    let mut text = format!("error: {}\n", error.message);
    if error.kind == ErrorKind::Usage {
        text.push_str(&format!(
            "Run '{} {} --help' for detailed usage.\n",
            bin.name, leaf.name
        ));
    }
    terminal.stderr.write(&text);
    1
}
