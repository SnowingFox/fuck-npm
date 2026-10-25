import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createColors } from "picocolors";
import { afterAll, describe, expect, test } from "vitest";
import type { CoreEvent, InstallCommand, InstallSummary } from "@/core/types.ts";
import { CoreError } from "@/errors.ts";
import { createProgress } from "@/output/progress.ts";
import { formatInstallSummary } from "@/output/summary.ts";
import { testBed } from "../harness.ts";

/**
 * Golden fixtures shared with the native launcher (`crates/fnpm-cli/tests/fixtures.rs`).
 * The TS CLI is the reference: these tests pin its output, and the Rust tests require the
 * native renderer and parser to produce the same bytes. Regenerate the `expected` fields
 * after an intentional change with `UPDATE_NATIVE_FIXTURES=1`.
 */

const UPDATE = process.env.UPDATE_NATIVE_FIXTURES === "1";
const fixture = (name: string): string =>
  fileURLToPath(new URL(`../../../../../crates/fnpm-cli/tests/fixtures/${name}`, import.meta.url));

function load<T>(name: string): { path: string; data: { cases: T[] } & Record<string, unknown> } {
  const path = fixture(name);
  return { path, data: JSON.parse(readFileSync(path, "utf8")) };
}

function check(actual: unknown, expected: unknown, assign: (value: unknown) => void): void {
  if (UPDATE) {
    assign(actual);
    return;
  }
  expect(actual).toEqual(expected);
}

type CliCase = {
  name: string;
  args: string[];
  env?: Record<string, string>;
  stdoutIsTTY?: boolean;
  stderrIsTTY?: boolean;
  columns?: number;
  events?: CoreEvent[];
  summary?: InstallSummary;
  coreError?: { code: string; message: string };
  expected?: unknown;
};

const cli = load<CliCase>("cli-cases.json");

describe("cli-cases.json", () => {
  for (const testCase of cli.data.cases) {
    test(testCase.name, async () => {
      const summary = testCase.summary ?? (cli.data.defaultSummary as InstallSummary);
      const bed = testBed({
        cwd: cli.data.cwd as string,
        env: testCase.env ?? {},
        stdoutIsTTY: testCase.stdoutIsTTY ?? false,
        stderrIsTTY: testCase.stderrIsTTY ?? false,
        columns: testCase.columns ?? 120,
        core: {
          install: (_options, emit) => {
            for (const event of testCase.events ?? []) emit(event);
            if (testCase.coreError) throw new CoreError(testCase.coreError.message, testCase.coreError.code);
            return summary;
          },
        },
      });
      const exit = await bed.run(testCase.args);
      const actual = {
        exit,
        stdout: bed.stdout.text(),
        stderr: bed.stderr.text(),
        install: bed.core.installs[0] ?? null,
      };
      check(actual, testCase.expected, (value) => (testCase.expected = value));
    });
  }
});

type SummaryCase = {
  name: string;
  command: InstallCommand;
  summary: InstallSummary;
  colors: boolean;
  arrows: boolean;
  expected?: string;
};

const summaries = load<SummaryCase>("summary.json");

describe("summary.json", () => {
  for (const testCase of summaries.data.cases) {
    test(testCase.name, () => {
      const lines = formatInstallSummary(testCase.summary, testCase.command, createColors(testCase.colors), {
        color: testCase.arrows,
      });
      check(lines.map((line) => `${line}\n`).join(""), testCase.expected, (value) => (testCase.expected = value as string));
    });
  }
});

type ProgressStep = { event: CoreEvent } | { advance: number } | { stop: true };
type ProgressCase = { name: string; columns: number; colors: boolean; steps: ProgressStep[]; expected?: string };

const progress = load<ProgressCase>("progress.json");

describe("progress.json", () => {
  for (const testCase of progress.data.cases) {
    test(testCase.name, () => {
      const bed = testBed({
        stderrIsTTY: true,
        columns: testCase.columns,
        env: testCase.colors ? { FORCE_COLOR: "1" } : { NO_COLOR: "1" },
      });
      const renderer = createProgress(bed.runtime);
      for (const step of testCase.steps) {
        if ("event" in step) renderer.sink(step.event);
        else if ("advance" in step) bed.advance(step.advance);
        else renderer.stop();
      }
      check(bed.stderr.text(), testCase.expected, (value) => (testCase.expected = value as string));
    });
  }
});

afterAll(() => {
  if (!UPDATE) return;
  for (const file of [cli, summaries, progress]) {
    writeFileSync(file.path, `${JSON.stringify(file.data, null, 2)}\n`);
  }
});
