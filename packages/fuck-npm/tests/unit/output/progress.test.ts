import { describe, expect, test } from "vitest";
import { createProgress, formatBytes, THROTTLE_MS, wantsProgress } from "@/output/progress.ts";
import { plain, testBed } from "../harness.ts";

const draws = (text: string): number => text.split("\r\u001B[2K").length - 1;

describe("wantsProgress", () => {
  const flags = { silent: false, quiet: false, noProgress: false };

  test("only on a TTY stderr", () => {
    expect(wantsProgress(testBed({ stderrIsTTY: true }).runtime, flags)).toBe(true);
    expect(wantsProgress(testBed({ stderrIsTTY: false }).runtime, flags)).toBe(false);
  });

  test.each([
    [{ CI: "true" }],
    [{ GITHUB_ACTIONS: "true" }],
    [{ FNPM_PROGRESS: "0" }],
    [{ BUN_INSTALL_PROGRESS: "0" }],
  ])("off with %j", (env) => {
    expect(wantsProgress(testBed({ stderrIsTTY: true, env }).runtime, flags)).toBe(false);
  });

  test("off with --quiet, --silent or --no-progress", () => {
    const { runtime } = testBed({ stderrIsTTY: true });
    expect(wantsProgress(runtime, { ...flags, quiet: true })).toBe(false);
    expect(wantsProgress(runtime, { ...flags, silent: true })).toBe(false);
    expect(wantsProgress(runtime, { ...flags, noProgress: true })).toBe(false);
  });
});

describe("createProgress", () => {
  test(`draws at most once per ${THROTTLE_MS}ms, with a trailing redraw`, () => {
    const bed = testBed({ stderrIsTTY: true });
    const progress = createProgress(bed.runtime);

    for (let resolved = 1; resolved <= 100; resolved += 1) {
      progress.sink({ type: "resolve", resolved, total: 100 });
    }
    // The first event draws immediately; the burst after it is coalesced.
    expect(draws(bed.stderr.text())).toBe(1);

    bed.advance(THROTTLE_MS);
    expect(draws(bed.stderr.text())).toBe(2);
    expect(plain(bed.stderr.text())).toContain("Resolving 100/100");
    progress.stop();
  });

  test("shows download bytes and extract counts", () => {
    const bed = testBed({ stderrIsTTY: true });
    const progress = createProgress(bed.runtime);

    progress.sink({ type: "download", done: 2, total: 5, bytes: 3 * 1024 * 1024 });
    bed.advance(THROTTLE_MS);
    progress.sink({ type: "extract", done: 1, total: 5 });
    bed.advance(THROTTLE_MS);

    expect(plain(bed.stderr.text())).toContain("Downloading 2/5 3.0 MiB  Extracted 1/5");
    progress.stop();
  });

  test("warnings print above the line", () => {
    const bed = testBed({ stderrIsTTY: true });
    const progress = createProgress(bed.runtime);

    progress.sink({ type: "link", done: 1, total: 2 });
    progress.sink({ type: "warn", message: "deprecated: request" });

    expect(plain(bed.stderr.text())).toContain("warn: deprecated: request\n");
    progress.stop();
  });

  test("stop erases the line and cancels the pending redraw", () => {
    const bed = testBed({ stderrIsTTY: true });
    const progress = createProgress(bed.runtime);

    progress.sink({ type: "resolve", resolved: 1, total: 2 });
    progress.sink({ type: "resolve", resolved: 2, total: 2 });
    progress.stop();
    const after = bed.stderr.text();
    bed.advance(THROTTLE_MS * 2);

    expect(after.endsWith("\r\u001B[2K")).toBe(true);
    expect(bed.stderr.text()).toBe(after);
  });

  test("never wraps a narrow terminal", () => {
    const bed = testBed({ stderrIsTTY: true, columns: 21 });
    const progress = createProgress(bed.runtime);

    progress.sink({ type: "download", done: 1000, total: 2000, bytes: 999_999_999 });
    const line = plain(bed.stderr.text()).replace("\r", "");
    expect(line.length).toBeLessThanOrEqual(20);
    progress.stop();
  });
});

describe("formatBytes", () => {
  test.each([
    [512, "512 B"],
    [2048, "2.0 KiB"],
    [5 * 1024 * 1024, "5.0 MiB"],
  ])("%d → %s", (bytes, text) => {
    expect(formatBytes(bytes)).toBe(text);
  });
});
