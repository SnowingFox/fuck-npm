//! The terminal progress renderer on stderr and the diagnostics-only sink: a port of
//! `packages/fuck-npm/src/output/progress.ts`.
//!
//! [`Renderer`] is the renderer's state machine with time passed in, so the shared golden
//! fixtures drive it with the same manual clock as the TS tests. [`ProgressSink`] serializes
//! producer events with timer redraws and keeps long-running tasks visibly alive.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fnpm_types::text::escape_control_chars_in_lines;
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

#[derive(Debug)]
struct ActiveScript {
    id: Option<String>,
    package: String,
    script: String,
    started: f64,
    foreground: bool,
}

#[derive(Debug)]
struct Telemetry {
    cpus: u32,
    io_threads: u32,
    link_threads: u32,
    network_active: u32,
    network_limit: u32,
    bytes: u64,
    requests: u64,
    cache_hits: u64,
    retries: u64,
    reused_packages: u64,
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
    current: String,
    scripts: Vec<ActiveScript>,
    scripts_done: u32,
    scripts_total: u32,
    scripts_failed: u32,
    telemetry: Option<Telemetry>,
    sample_at: f64,
    sample_bytes: u64,
    speed: u64,
    last_draw: f64,
    drawn_lines: usize,
    pending: Option<f64>,
    stopped: bool,
    output_active: bool,
}

const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub const HEARTBEAT_MS: f64 = 100.0;

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn seconds(ms: f64) -> String {
    format!("{}s", to_fixed(ms.max(0.0) / 1000.0, 1))
}

fn count(done: u32, total: u32) -> String {
    format!(
        "{done}/{}",
        if total == 0 {
            "?".to_owned()
        } else {
            total.to_string()
        }
    )
}

impl Renderer {
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
            current: String::new(),
            scripts: Vec::new(),
            scripts_done: 0,
            scripts_total: 0,
            scripts_failed: 0,
            telemetry: None,
            sample_at: 0.0,
            sample_bytes: 0,
            speed: 0,
            last_draw: f64::NEG_INFINITY,
            drawn_lines: 0,
            pending: Some(HEARTBEAT_MS),
            stopped: false,
            output_active: false,
        }
    }

    pub fn deadline(&self) -> Option<f64> {
        self.pending
    }

    fn paused(&self) -> bool {
        self.output_active || self.scripts.iter().any(|script| script.foreground)
    }

    fn clear(&mut self, out: &mut String) {
        if self.drawn_lines == 0 {
            return;
        }
        out.push_str(ERASE);
        for _ in 1..self.drawn_lines {
            out.push_str("\x1b[1A");
            out.push_str(ERASE);
        }
        self.drawn_lines = 0;
    }

    fn lines(&self, now: f64) -> Vec<String> {
        let pc = self.pc;
        let phase = if self.scripts.is_empty() {
            self.phase
        } else {
            Phase::Script
        };
        let main = match phase {
            Phase::Resolve => format!(
                "{} {}",
                pc.cyan("Resolving"),
                count(self.resolved, self.resolve_total)
            ),
            Phase::Download | Phase::Extract => format!(
                "{} {} {}  {} {}",
                pc.cyan("Downloading"),
                count(self.downloaded, self.download_total),
                pc.dim(&format_bytes(self.bytes)),
                pc.cyan("Extracted"),
                count(
                    self.extracted,
                    if self.extract_total == 0 {
                        self.download_total
                    } else {
                        self.extract_total
                    }
                )
            ),
            Phase::Link => format!(
                "{} {}",
                pc.cyan("Installing"),
                count(self.linked, self.link_total)
            ),
            Phase::Script => format!(
                "{} {}  {} {}{}",
                pc.cyan(if self.link_total > 0 {
                    "Installed"
                } else {
                    "Resolved"
                }),
                if self.link_total > 0 {
                    count(self.linked, self.link_total)
                } else {
                    count(self.resolved, self.resolve_total)
                },
                pc.cyan("Scripts"),
                count(self.scripts_done, self.scripts_total),
                if self.scripts_failed > 0 {
                    format!(" ({} failed)", self.scripts_failed)
                } else {
                    String::new()
                }
            ),
        };
        let frame = FRAMES[(now / HEARTBEAT_MS).floor() as usize % FRAMES.len()].to_string();
        let mut lines = vec![format!(
            "{} {main}  {}",
            pc.cyan(&frame),
            pc.dim(&seconds(now))
        )];
        if phase == Phase::Script {
            lines.push(match self.scripts.first() {
                Some(active) => format!(
                    "  {} {} {} {}{}",
                    pc.cyan("Running"),
                    clean(&active.package),
                    pc.dim(&clean(&active.script)),
                    seconds(now - active.started),
                    if self.scripts.len() > 1 {
                        format!(" (+{} running)", self.scripts.len() - 1)
                    } else {
                        String::new()
                    }
                ),
                None => format!("  {}", pc.dim("Finalizing install")),
            });
        } else if !self.current.is_empty() {
            lines.push(format!("  {}", clean(&self.current)));
        }
        if let Some(t) = &self.telemetry {
            let rate = if now - self.sample_at > 1500.0 {
                0
            } else {
                self.speed
            };
            if t.reused_packages > 0 {
                lines.push(pc.dim(&format!("  Store {} packages reused", t.reused_packages)));
            }
            let network = if t.network_active > 0 {
                format!(
                    "Network {}/{} · {}/s",
                    t.network_active,
                    t.network_limit,
                    format_bytes(rate)
                )
            } else {
                "Network idle".to_owned()
            };
            let traffic = if t.requests > 0 || t.bytes > 0 || t.network_active > 0 {
                format!(" · {} · {} requests", format_bytes(t.bytes), t.requests)
            } else {
                " · no registry requests".to_owned()
            };
            lines.push(pc.dim(&format!(
                "  {network}{traffic}{}{}",
                if t.cache_hits > 0 {
                    format!(" · {} metadata cached", t.cache_hits)
                } else {
                    String::new()
                },
                if t.retries > 0 {
                    format!(" · {} retries", t.retries)
                } else {
                    String::new()
                }
            )));
            lines.push(pc.dim(&format!(
                "  {} CPU cores · workers: {} I/O, {} link",
                t.cpus, t.io_threads, t.link_threads
            )));
        }
        lines
    }

    fn draw(&mut self, now: f64, out: &mut String) {
        self.pending = None;
        if self.stopped || self.paused() {
            return;
        }
        self.last_draw = now;
        self.clear(out);
        let width = self.columns.saturating_sub(1).max(1);
        let lines = self.lines(now);
        out.push_str(ERASE);
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let visible = strip_ansi(line);
            if visible.chars().count() > width {
                out.extend(visible.chars().take(width));
            } else {
                out.push_str(line);
            }
        }
        self.drawn_lines = lines.len();
        self.pending = Some(now + HEARTBEAT_MS);
    }

    fn schedule(&mut self, now: f64, out: &mut String) {
        if self.stopped || self.paused() {
            return;
        }
        let delay = THROTTLE_MS - (now - self.last_draw);
        if delay <= 0.0 {
            self.draw(now, out);
        } else {
            self.pending = Some(now + delay);
        }
    }

    fn above(&mut self, message: &str, now: f64, out: &mut String) {
        self.clear(out);
        out.push_str(message);
        out.push('\n');
        self.schedule(now, out);
    }

    pub fn event(&mut self, event: &Event, now: f64, out: &mut String) {
        if self.stopped {
            return;
        }
        match event {
            Event::Resolve {
                resolved,
                total,
                name,
            } => {
                self.phase = Phase::Resolve;
                self.resolved = *resolved;
                self.resolve_total = *total;
                self.current = name.clone().unwrap_or_default();
            }
            Event::Download {
                done,
                total,
                bytes,
                name,
            } => {
                self.phase = Phase::Download;
                self.downloaded = *done;
                self.download_total = *total;
                self.current = name.clone().unwrap_or_default();
                if let Some(bytes) = bytes {
                    self.bytes = *bytes;
                }
            }
            Event::Extract { done, total, name } => {
                if self.phase == Phase::Resolve {
                    self.phase = Phase::Extract;
                }
                self.extracted = *done;
                self.extract_total = *total;
                if let Some(name) = name {
                    self.current.clone_from(name);
                }
            }
            Event::Link { done, total, name } => {
                self.phase = Phase::Link;
                self.linked = *done;
                self.link_total = *total;
                self.current = name.clone().unwrap_or_default();
            }
            Event::ScriptPlan { total } => self.scripts_total += total,
            Event::Script {
                package,
                script,
                phase,
                exit_code,
                id,
                foreground,
            } => {
                self.phase = Phase::Script;
                match phase {
                    fnpm_types::event::ScriptPhase::Start => {
                        self.scripts.push(ActiveScript {
                            id: id.clone(),
                            package: package.clone(),
                            script: script.clone(),
                            started: now,
                            foreground: *foreground,
                        });
                        if self.paused() {
                            self.pending = None;
                            self.clear(out);
                        }
                    }
                    fnpm_types::event::ScriptPhase::End => {
                        if let Some(index) = self.scripts.iter().position(|active| match id {
                            Some(id) => active.id.as_ref() == Some(id),
                            None => active.package == *package && active.script == *script,
                        }) {
                            self.scripts.remove(index);
                            self.scripts_done += 1;
                            if *exit_code != Some(0) {
                                self.scripts_failed += 1;
                            }
                        }
                    }
                }
            }
            Event::Telemetry {
                cpus,
                io_threads,
                link_threads,
                network_active,
                network_limit,
                bytes,
                requests,
                cache_hits,
                retries,
                reused_packages,
            } => {
                if now > self.sample_at {
                    self.speed = (bytes.saturating_sub(self.sample_bytes) as f64 * 1000.0
                        / (now - self.sample_at))
                        .floor() as u64;
                }
                self.sample_at = now;
                self.sample_bytes = *bytes;
                self.telemetry = Some(Telemetry {
                    cpus: *cpus,
                    io_threads: *io_threads,
                    link_threads: *link_threads,
                    network_active: *network_active,
                    network_limit: *network_limit,
                    bytes: *bytes,
                    requests: *requests,
                    cache_hits: *cache_hits,
                    retries: *retries,
                    reused_packages: *reused_packages,
                });
            }
            Event::Output { active } => {
                self.output_active = *active;
                if *active {
                    self.pending = None;
                    self.clear(out);
                }
            }
            Event::Warn { message, .. } => {
                let message = escape_control_chars_in_lines(message);
                self.above(&format!("{} {message}", self.pc.yellow("warn:")), now, out);
                return;
            }
            Event::Error { message, .. } => {
                let message = escape_control_chars_in_lines(message);
                self.above(&format!("{} {message}", self.pc.red("error:")), now, out);
                return;
            }
            Event::Notice { message } => {
                self.above(&self.pc.dim(&clean(message)), now, out);
                return;
            }
            Event::Status { .. } | Event::Summary { .. } => return,
        }
        self.schedule(now, out);
    }

    pub fn fire(&mut self, now: f64, out: &mut String) {
        if self.pending.is_some_and(|due| due <= now) {
            self.draw(now, out);
        }
    }

    pub fn stop(&mut self, out: &mut String) {
        self.stopped = true;
        self.pending = None;
        self.clear(out);
    }
}

struct Shared {
    renderer: Renderer,
    /// Set by `stop`; tells the timer thread to exit.
    done: bool,
}

/// The live progress region: an [`EventSink`] for the core plus a timer thread for the
/// heartbeat. Writes go straight to stderr under the state lock, so lines never
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
    fn wants_progress(&self) -> bool {
        true
    }

    fn emit(&self, event: Event) {
        let now = self.now();
        let (lock, cond) = &*self.state;
        let mut shared = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut out = String::new();
        shared.renderer.event(&event, now, &mut out);
        if !out.is_empty() {
            self.stderr.write(&out);
        }
        cond.notify_all();
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
            // A package, a registry or a config file wrote most of what these say.
            Event::Warn { message, .. } => Some(format!(
                "{} {}\n",
                self.pc.yellow("warn:"),
                escape_control_chars_in_lines(message)
            )),
            Event::Error { message, .. } => Some(format!(
                "{} {}\n",
                self.pc.red("error:"),
                escape_control_chars_in_lines(message)
            )),
            Event::Status { message } => Some(format!("{message}\n")),
            Event::Notice { message } => Some(format!("{}\n", self.pc.dim(message))),
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
        assert_eq!(out.matches("Resolving").count(), 2);
        assert!(out.contains("Resolving 100/100"));
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
