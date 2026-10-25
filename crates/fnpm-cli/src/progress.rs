//! The single-line progress renderer on stderr and the diagnostics-only sink: a port of
//! `packages/fuck-npm/src/output/progress.ts`.
//!
//! [`Renderer`] is the renderer's state machine with time passed in, so the shared golden
//! fixtures can drive it exactly like the TS tests drive theirs (manual clock, one pending
//! timer). [`ProgressSink`] wraps it for real use: core threads call `emit`, and a timer
//! thread performs the trailing redraw.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fnpm_types::{Event, EventSink};

use crate::color::{Colors, strip_ansi, to_fixed};
use crate::terminal::{Env, Stream};

/// Redraw at most this often.
pub const THROTTLE_MS: f64 = 50.0;

const ERASE: &str = "\r\x1b[2K";

/// The CI markers bun checks (`fnpm_config::Env::is_ci`).
const CI_VARS: [&str; 5] = [
    "CI",
    "TDDIUM",
    "GITHUB_ACTIONS",
    "JENKINS_URL",
    "bamboo.buildKey",
];

/// `wantsProgress`: only on a TTY stderr, not quiet/silent/--no-progress, not disabled by
/// `FNPM_PROGRESS=0` (`BUN_INSTALL_PROGRESS=0` as fallback), and not on CI.
pub fn wants_progress(
    env: &Env,
    stderr_is_tty: bool,
    silent: bool,
    quiet: bool,
    no_progress: bool,
) -> bool {
    if !stderr_is_tty || silent || quiet || no_progress {
        return false;
    }
    if env
        .get("FNPM_PROGRESS")
        .or_else(|| env.get("BUN_INSTALL_PROGRESS"))
        == Some("0")
    {
        return false;
    }
    !CI_VARS.iter().any(|key| {
        env.get(key)
            .is_some_and(|value| !value.is_empty() && value != "0" && value != "false")
    })
}

/// `formatBytes`.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KiB", to_fixed(bytes as f64 / 1024.0, 1))
    } else {
        format!("{} MiB", to_fixed(bytes as f64 / 1024.0 / 1024.0, 1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Resolve,
    Download,
    Extract,
    Link,
    Script,
}

/// The renderer state. Every method appends what it would write to stderr to `out`.
#[derive(Debug)]
pub struct Renderer {
    pc: Colors,
    columns: usize,
    phase: Phase,
    resolved: u32,
    resolve_total: u32,
    downloaded: u32,
    download_total: u32,
    bytes: u64,
    extracted: u32,
    extract_total: u32,
    linked: u32,
    link_total: u32,
    script: Option<String>,
    last_draw: f64,
    drawn: bool,
    /// When the pending trailing redraw is due (ms on the caller's clock).
    pending: Option<f64>,
    stopped: bool,
}

impl Renderer {
    /// A renderer for a terminal `columns` wide, colouring with `pc`.
    pub fn new(pc: Colors, columns: usize) -> Self {
        Self {
            pc,
            columns,
            phase: Phase::Resolve,
            resolved: 0,
            resolve_total: 0,
            downloaded: 0,
            download_total: 0,
            bytes: 0,
            extracted: 0,
            extract_total: 0,
            linked: 0,
            link_total: 0,
            script: None,
            last_draw: f64::NEG_INFINITY,
            drawn: false,
            pending: None,
            stopped: false,
        }
    }

    /// When the pending redraw is due, if one is scheduled.
    pub fn deadline(&self) -> Option<f64> {
        self.pending
    }

    fn text(&self) -> String {
        let pc = self.pc;
        let or_question = |total: u32| {
            if total == 0 {
                "?".to_owned()
            } else {
                total.to_string()
            }
        };
        match self.phase {
            Phase::Resolve => format!(
                "{} {}/{}",
                pc.cyan("Resolving"),
                self.resolved,
                or_question(self.resolve_total)
            ),
            Phase::Download | Phase::Extract => format!(
                "{} {}/{} {}  {} {}/{}",
                pc.cyan("Downloading"),
                self.downloaded,
                self.download_total,
                pc.dim(&format_bytes(self.bytes)),
                pc.cyan("Extracted"),
                self.extracted,
                if self.extract_total == 0 {
                    self.download_total
                } else {
                    self.extract_total
                }
            ),
            Phase::Link => format!("{} {}/{}", pc.cyan("Linking"), self.linked, self.link_total),
            Phase::Script => format!(
                "{} {}",
                pc.cyan("Running"),
                self.script.as_deref().unwrap_or("")
            ),
        }
    }

    fn draw(&mut self, now: f64, out: &mut String) {
        self.pending = None;
        if self.stopped {
            return;
        }
        self.last_draw = now;
        let width = self.columns.saturating_sub(1).max(20);
        let line = self.text();
        // Never wrap: a wrapped line cannot be erased with \r.
        let visible = strip_ansi(&line);
        out.push_str(ERASE);
        if visible.chars().count() > width {
            out.extend(visible.chars().take(width));
        } else {
            out.push_str(&line);
        }
        self.drawn = true;
    }

    fn schedule(&mut self, now: f64, out: &mut String) {
        if self.stopped || self.pending.is_some() {
            return;
        }
        let elapsed = now - self.last_draw;
        if elapsed >= THROTTLE_MS {
            self.draw(now, out);
            return;
        }
        self.pending = Some(now + (THROTTLE_MS - elapsed));
    }

    fn above(&mut self, message: &str, now: f64, out: &mut String) {
        if self.drawn {
            out.push_str(ERASE);
        }
        out.push_str(message);
        out.push('\n');
        self.drawn = false;
        self.schedule(now, out);
    }

    /// One core event at time `now`.
    pub fn event(&mut self, event: &Event, now: f64, out: &mut String) {
        match event {
            Event::Resolve {
                resolved, total, ..
            } => {
                self.phase = Phase::Resolve;
                self.resolved = *resolved;
                self.resolve_total = *total;
            }
            Event::Download {
                done, total, bytes, ..
            } => {
                self.phase = Phase::Download;
                self.downloaded = *done;
                self.download_total = *total;
                if let Some(bytes) = bytes {
                    self.bytes = *bytes;
                }
            }
            Event::Extract { done, total, .. } => {
                if self.phase == Phase::Resolve {
                    self.phase = Phase::Extract;
                }
                self.extracted = *done;
                self.extract_total = *total;
            }
            Event::Link { done, total } => {
                self.phase = Phase::Link;
                self.linked = *done;
                self.link_total = *total;
            }
            Event::Script {
                package, script, ..
            } => {
                self.phase = Phase::Script;
                self.script = Some(format!("{package} {}", self.pc.dim(script)));
            }
            Event::Warn { message, .. } => {
                let line = format!("{} {message}", self.pc.yellow("warn:"));
                self.above(&line, now, out);
                return;
            }
            Event::Error { message, .. } => {
                let line = format!("{} {message}", self.pc.red("error:"));
                self.above(&line, now, out);
                return;
            }
            Event::Status { .. } | Event::Summary { .. } => return,
        }
        self.schedule(now, out);
    }

    /// The pending timer fired at `now`.
    pub fn fire(&mut self, now: f64, out: &mut String) {
        if self.pending.is_some() {
            self.draw(now, out);
        }
    }

    /// Erase the line and cancel the pending redraw. Call before printing the summary.
    pub fn stop(&mut self, out: &mut String) {
        self.stopped = true;
        self.pending = None;
        if self.drawn {
            out.push_str(ERASE);
            self.drawn = false;
        }
    }
}

struct Shared {
    renderer: Renderer,
    /// Set by `stop`; tells the timer thread to exit.
    done: bool,
}

/// The live progress line: an [`EventSink`] for the core plus a timer thread for the
/// trailing redraw. Writes go straight to stderr under the state lock, so lines never
/// interleave.
pub struct ProgressSink {
    state: Arc<(Mutex<Shared>, Condvar)>,
    stderr: Arc<dyn Stream>,
    epoch: Instant,
    timer: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ProgressSink {
    /// Start rendering to `stderr`.
    pub fn start(pc: Colors, columns: usize, stderr: Arc<dyn Stream>) -> Arc<Self> {
        let state = Arc::new((
            Mutex::new(Shared {
                renderer: Renderer::new(pc, columns),
                done: false,
            }),
            Condvar::new(),
        ));
        let epoch = Instant::now();
        let thread_state = Arc::clone(&state);
        let thread_stderr = Arc::clone(&stderr);
        let timer = std::thread::Builder::new()
            .name("fnpm-progress".into())
            .spawn(move || timer_loop(&thread_state, &*thread_stderr, epoch))
            .ok();
        Arc::new(Self {
            state,
            stderr,
            epoch,
            timer: Mutex::new(timer),
        })
    }

    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// Erase the line and stop the timer thread.
    pub fn stop(&self) {
        {
            let (lock, cond) = &*self.state;
            let mut shared = lock.lock().unwrap_or_else(|poison| poison.into_inner());
            let mut out = String::new();
            shared.renderer.stop(&mut out);
            shared.done = true;
            self.stderr.write(&out);
            cond.notify_all();
        }
        if let Some(handle) = self
            .timer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

fn timer_loop(state: &(Mutex<Shared>, Condvar), stderr: &dyn Stream, epoch: Instant) {
    let (lock, cond) = state;
    let mut shared = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    loop {
        if shared.done {
            return;
        }
        let now = epoch.elapsed().as_secs_f64() * 1000.0;
        match shared.renderer.deadline() {
            Some(due) if due <= now => {
                let mut out = String::new();
                shared.renderer.fire(now, &mut out);
                stderr.write(&out);
            }
            Some(due) => {
                let wait = Duration::from_secs_f64(((due - now) / 1000.0).max(0.0));
                shared = cond
                    .wait_timeout(shared, wait)
                    .unwrap_or_else(|poison| poison.into_inner())
                    .0;
            }
            None => {
                shared = cond
                    .wait(shared)
                    .unwrap_or_else(|poison| poison.into_inner());
            }
        }
    }
}

impl EventSink for ProgressSink {
    fn emit(&self, event: Event) {
        let now = self.now();
        let (lock, cond) = &*self.state;
        let mut shared = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut out = String::new();
        let had_deadline = shared.renderer.deadline().is_some();
        shared.renderer.event(&event, now, &mut out);
        if !out.is_empty() {
            self.stderr.write(&out);
        }
        if !had_deadline && shared.renderer.deadline().is_some() {
            cond.notify_all();
        }
    }
}

impl Drop for ProgressSink {
    fn drop(&mut self) {
        let (lock, cond) = &*self.state;
        if let Ok(mut shared) = lock.lock() {
            shared.done = true;
        }
        cond.notify_all();
    }
}

/// Event handling when no progress line is drawn (piped, CI, `--quiet`): warnings and
/// errors still reach stderr, counters are dropped (`diagnosticsSink`).
pub struct DiagnosticsSink {
    pc: Colors,
    stderr: Arc<dyn Stream>,
}

impl DiagnosticsSink {
    pub fn new(pc: Colors, stderr: Arc<dyn Stream>) -> Self {
        Self { pc, stderr }
    }

    /// The line an event prints, if any.
    pub fn line(&self, event: &Event) -> Option<String> {
        match event {
            Event::Warn { message, .. } => Some(format!("{} {message}\n", self.pc.yellow("warn:"))),
            Event::Error { message, .. } => Some(format!("{} {message}\n", self.pc.red("error:"))),
            Event::Status { message } => Some(format!("{message}\n")),
            _ => None,
        }
    }
}

impl EventSink for DiagnosticsSink {
    fn emit(&self, event: Event) {
        if let Some(line) = self.line(&event) {
            self.stderr.write(&line);
        }
    }

    fn enabled(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(resolved: u32, total: u32) -> Event {
        Event::Resolve {
            resolved,
            total,
            name: None,
        }
    }

    #[test]
    fn throttles_with_a_trailing_redraw() {
        let mut renderer = Renderer::new(Colors::new(false), 120);
        let mut out = String::new();
        for resolved in 1..=100 {
            renderer.event(&resolve(resolved, 100), 0.0, &mut out);
        }
        assert_eq!(out.matches(ERASE).count(), 1);
        assert_eq!(renderer.deadline(), Some(THROTTLE_MS));
        renderer.fire(THROTTLE_MS, &mut out);
        assert_eq!(out.matches(ERASE).count(), 2);
        assert!(out.ends_with("Resolving 100/100"));
    }

    #[test]
    fn truncates_to_the_terminal() {
        let mut renderer = Renderer::new(Colors::new(true), 21);
        let mut out = String::new();
        renderer.event(
            &Event::Download {
                done: 1000,
                total: 2000,
                bytes: Some(999_999_999),
                name: None,
            },
            0.0,
            &mut out,
        );
        let line = out.trim_start_matches(ERASE);
        assert_eq!(line.chars().count(), 20);
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn env_rules() {
        let env = Env::from_pairs([("CI", "true")]);
        assert!(!wants_progress(&env, true, false, false, false));
        let env = Env::from_pairs([
            ("CI", "false"),
            ("FNPM_PROGRESS", "1"),
            ("BUN_INSTALL_PROGRESS", "0"),
        ]);
        assert!(wants_progress(&env, true, false, false, false));
        let env = Env::from_pairs([("BUN_INSTALL_PROGRESS", "0")]);
        assert!(!wants_progress(&env, true, false, false, false));
        assert!(!wants_progress(&Env::default(), false, false, false, false));
        assert!(!wants_progress(&Env::default(), true, false, true, false));
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(1280), "1.3 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
