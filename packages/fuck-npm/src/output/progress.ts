import { createColors } from "picocolors";
import type { CoreEvent, EventSink } from "../core/types.ts";
import type { CliRuntime } from "../runtime/types.ts";
import { colorEnabled } from "./output.ts";

export const THROTTLE_MS = 50;
export const HEARTBEAT_MS = 100;

export type Progress = {
  sink: EventSink;
  stop(): void;
};

type Phase = "resolve" | "download" | "extract" | "link" | "script";
type ActiveScript = { id?: string; package: string; script: string; started: number; foreground: boolean };
type Telemetry = Extract<CoreEvent, { type: "telemetry" }>;
const ERASE = "\r\u001B[2K";
const ANSI_RE = /\u001B\[[0-9;]*m/g;
const FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const CI_VARS = ["CI", "TDDIUM", "GITHUB_ACTIONS", "JENKINS_URL", "bamboo.buildKey"] as const;
const clean = (text: string): string => text.replaceAll(/[\u0000-\u001F\u007F-\u009F]/g, " ");
const seconds = (ms: number): string => `${(Math.max(0, ms) / 1000).toFixed(1)}s`;
const count = (done: number, total: number): string => `${done}/${total || "?"}`;

export function wantsProgress(runtime: CliRuntime, opts: { silent: boolean; quiet: boolean; noProgress: boolean }): boolean {
  if (!runtime.stderrIsTTY || opts.silent || opts.quiet || opts.noProgress) return false;
  const { env } = runtime;
  if ((env.FNPM_PROGRESS ?? env.BUN_INSTALL_PROGRESS) === "0") return false;
  return !CI_VARS.some((key) => {
    const value = env[key];
    return value !== undefined && value !== "" && value !== "0" && value !== "false";
  });
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

/** Event bursts are throttled; the heartbeat keeps slow network and scripts visibly alive. */
export function createProgress(runtime: CliRuntime): Progress {
  const pc = createColors(colorEnabled(runtime, runtime.stderrIsTTY));
  const started = runtime.now();
  let phase: Phase = "resolve";
  let resolved = 0, resolveTotal = 0, downloaded = 0, downloadTotal = 0;
  let bytes = 0, extracted = 0, extractTotal = 0, linked = 0, linkTotal = 0;
  let current = "";
  const scripts: ActiveScript[] = [];
  let scriptsDone = 0, scriptsTotal = 0, scriptsFailed = 0;
  let telemetry: Telemetry | undefined;
  let sampleAt = started, sampleBytes = 0, speed = 0;
  let lastDraw = Number.NEGATIVE_INFINITY;
  let drawnLines = 0;
  let pending: { cancel(): void } | undefined;
  let stopped = false;
  let outputActive = false;
  const paused = (): boolean => outputActive || scripts.some((script) => script.foreground);

  const clear = (): void => {
    if (!drawnLines) return;
    runtime.stderr.write(ERASE + (`\u001B[1A${ERASE}`).repeat(drawnLines - 1));
    drawnLines = 0;
  };

  const lines = (now: number): string[] => {
    let main: string;
    const displayPhase = scripts.length ? "script" : phase;
    switch (displayPhase) {
      case "resolve": main = `${pc.cyan("Resolving")} ${count(resolved, resolveTotal)}`; break;
      case "download":
      case "extract": main = `${pc.cyan("Downloading")} ${count(downloaded, downloadTotal)} ${pc.dim(formatBytes(bytes))}  ${pc.cyan("Extracted")} ${count(extracted, extractTotal || downloadTotal)}`; break;
      case "link": main = `${pc.cyan("Installing")} ${count(linked, linkTotal)}`; break;
      case "script": main = `${pc.cyan(linkTotal ? "Installed" : "Resolved")} ${linkTotal ? count(linked, linkTotal) : count(resolved, resolveTotal)}  ${pc.cyan("Scripts")} ${count(scriptsDone, scriptsTotal)}${scriptsFailed ? ` (${scriptsFailed} failed)` : ""}`; break;
    }
    const result = [`${pc.cyan(FRAMES[Math.floor((now - started) / HEARTBEAT_MS) % FRAMES.length] ?? FRAMES[0]!)} ${main}  ${pc.dim(seconds(now - started))}`];
    if (displayPhase === "script") {
      const active = scripts[0];
      result.push(active
        ? `  ${pc.cyan("Running")} ${clean(active.package)} ${pc.dim(clean(active.script))} ${seconds(now - active.started)}${scripts.length > 1 ? ` (+${scripts.length - 1} running)` : ""}`
        : `  ${pc.dim("Finalizing install")}`);
    } else if (current) result.push(`  ${clean(current)}`);
    if (telemetry) {
      const rate = now - sampleAt > 1500 ? 0 : speed;
      if (telemetry.reusedPackages) result.push(pc.dim(`  Store ${telemetry.reusedPackages} packages reused`));
      const network = telemetry.networkActive
        ? `Network ${telemetry.networkActive}/${telemetry.networkLimit} · ${formatBytes(rate)}/s`
        : "Network idle";
      const traffic = telemetry.requests || telemetry.bytes || telemetry.networkActive
        ? ` · ${formatBytes(telemetry.bytes)} · ${telemetry.requests} requests`
        : " · no registry requests";
      result.push(pc.dim(`  ${network}${traffic}${telemetry.cacheHits ? ` · ${telemetry.cacheHits} metadata cached` : ""}${telemetry.retries ? ` · ${telemetry.retries} retries` : ""}`));
      result.push(pc.dim(`  ${telemetry.cpus} CPU cores · workers: ${telemetry.ioThreads} I/O, ${telemetry.linkThreads} link`));
    }
    return result;
  };

  const draw = (): void => {
    pending = undefined;
    if (stopped || paused()) return;
    const now = runtime.now();
    lastDraw = now;
    clear();
    const width = Math.max(1, runtime.columns() - 1);
    const output = lines(now).map((line) => {
      const visible = Array.from(line.replaceAll(ANSI_RE, ""));
      return visible.length > width ? visible.slice(0, width).join("") : line;
    });
    runtime.stderr.write(ERASE + output.join("\n"));
    drawnLines = output.length;
    pending = runtime.setTimer(draw, HEARTBEAT_MS);
  };
  const schedule = (): void => {
    if (stopped || paused()) return;
    pending?.cancel();
    const delay = THROTTLE_MS - (runtime.now() - lastDraw);
    if (delay <= 0) draw();
    else pending = runtime.setTimer(draw, delay);
  };
  const above = (message: string): void => {
    clear();
    runtime.stderr.write(`${message}\n`);
    schedule();
  };
  const sink: EventSink = (event) => {
    if (stopped) return;
    switch (event.type) {
      case "resolve":
        phase = "resolve";
        resolved = event.resolved; resolveTotal = event.total; current = event.name ?? "";
        break;
      case "download":
        phase = "download";
        downloaded = event.done; downloadTotal = event.total; current = event.name ?? "";
        if (event.bytes !== undefined) bytes = event.bytes;
        break;
      case "extract":
        if (phase === "resolve") phase = "extract";
        extracted = event.done; extractTotal = event.total;
        if (event.name) current = event.name;
        break;
      case "link":
        phase = "link";
        linked = event.done; linkTotal = event.total; current = event.name ?? "";
        break;
      case "scriptplan": scriptsTotal += event.total; break;
      case "script": {
        phase = "script";
        if (event.phase === "start") {
          scripts.push({ id: event.id, package: event.package, script: event.script, started: runtime.now(), foreground: event.foreground ?? false });
          if (paused()) { pending?.cancel(); pending = undefined; clear(); }
        } else {
          const index = scripts.findIndex((active) => event.id !== undefined
            ? active.id === event.id
            : active.package === event.package && active.script === event.script);
          if (index >= 0) { scripts.splice(index, 1); scriptsDone += 1; if (event.exitCode !== 0) scriptsFailed += 1; }
        }
        break;
      }
      case "telemetry": {
        const now = runtime.now();
        if (now > sampleAt) speed = Math.floor(Math.max(0, event.bytes - sampleBytes) * 1000 / (now - sampleAt));
        sampleAt = now; sampleBytes = event.bytes; telemetry = event;
        break;
      }
      case "output":
        outputActive = event.active;
        if (outputActive) { pending?.cancel(); pending = undefined; clear(); }
        break;
      case "warn": above(`${pc.yellow("warn:")} ${event.message}`); return;
      case "error": above(`${pc.red("error:")} ${event.message}`); return;
      case "status":
      case "summary": return;
    }
    schedule();
  };
  const stop = (): void => { stopped = true; pending?.cancel(); pending = undefined; clear(); };
  const exit = runtime.onExit?.(stop);
  pending = runtime.setTimer(draw, HEARTBEAT_MS);
  return { sink, stop() { exit?.cancel(); stop(); } };
}

export function diagnosticsSink(runtime: CliRuntime, silent: boolean): EventSink {
  const pc = createColors(colorEnabled(runtime, runtime.stderrIsTTY));
  return (event) => {
    if (silent) return;
    if (event.type === "warn") runtime.stderr.write(`${pc.yellow("warn:")} ${event.message}\n`);
    else if (event.type === "error") runtime.stderr.write(`${pc.red("error:")} ${event.message}\n`);
    else if (event.type === "status") runtime.stderr.write(`${event.message}\n`);
  };
}
