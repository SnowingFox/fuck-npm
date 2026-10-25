import { createColors } from "picocolors";
import { describe, expect, test } from "vitest";
import { emptySummary } from "@/core/fake.ts";
import { formatElapsed, formatInstallSummary } from "@/output/summary.ts";
import { plain } from "../harness.ts";

const pc = createColors(false);
const render = (...args: Parameters<typeof formatInstallSummary>) => formatInstallSummary(...args).join("\n");

describe("formatElapsed", () => {
  test.each([
    [12.345, "[12.35ms]"],
    [1500, "[1500.00ms]"],
    [1501, "[1.50s]"],
    [12_340, "[12.34s]"],
  ])("%d ms → %s", (ms, text) => {
    expect(formatElapsed(ms)).toBe(text);
  });
});

describe("formatInstallSummary", () => {
  test("added, updated and removed direct dependencies, then the count", () => {
    const text = render(
      emptySummary({
        added: [{ name: "react", version: "18.3.1", availableVersion: "19.0.0" }],
        updated: [{ name: "zod", from: "3.22.0", to: "3.23.8" }],
        removed: ["left-pad"],
        installedCount: 12,
        removedCount: 1,
        elapsedMs: 432.1,
      }),
      "install",
      pc,
    );

    expect(text).toBe(
      [
        "",
        "+ react@18.3.1 (v19.0.0 available)",
        "↑ zod 3.22.0 → 3.23.8",
        "- left-pad",
        "",
        "12 packages installed [432.10ms]",
        "Removed: 1",
      ].join("\n"),
    );
  });

  test("without colour the arrows fall back to ASCII", () => {
    const text = render(
      emptySummary({ updated: [{ name: "zod", from: "1.0.0", to: "2.0.0" }], installedCount: 1 }),
      "update",
      pc,
      { color: false },
    );
    expect(text).toContain("^ zod 1.0.0 -> 2.0.0");
  });

  test("add lists what each request resolved to, with binaries", () => {
    const text = render(
      emptySummary({
        requested: [
          { name: "typescript", version: "5.9.3", bins: ["tsc", "tsserver"] },
          { name: "fsevents", version: "2.3.3", bins: [], skipped: true },
        ],
        added: [{ name: "typescript", version: "5.9.3" }],
        installedCount: 1,
        elapsedMs: 80,
      }),
      "add",
      pc,
    );

    expect(text).toContain("installed typescript@5.9.3 with binaries:\n  - tsc\n  - tsserver");
    expect(text).toContain("skipped fsevents@2.3.3 (unsupported platform)");
    // add reports max(installed, requests).
    expect(text).toContain("2 packages installed [80.00ms]");
  });

  test("a no-op says what was checked", () => {
    const text = render(
      emptySummary({ checked: { installs: 120, packages: 98 }, noop: true, elapsedMs: 9 }),
      "install",
      pc,
    );
    expect(text).toContain("Checked 120 installs across 98 packages (no changes) [9.00ms]");
  });

  test("removal-only and blocked postinstalls", () => {
    const text = render(emptySummary({ removedCount: 2, blockedScripts: 1, elapsedMs: 3 }), "remove", pc);

    expect(text).toContain("2 packages removed [3.00ms]");
    expect(text).toContain("Blocked 1 postinstall. Run `fnpm pm untrusted` for details.");
  });

  test("coloured output carries the same text", () => {
    const coloured = render(emptySummary({ installedCount: 1, elapsedMs: 1 }), "install", createColors(true));
    expect(plain(coloured)).toBe(render(emptySummary({ installedCount: 1, elapsedMs: 1 }), "install", pc));
  });
});
