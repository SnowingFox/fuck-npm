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
  test("added and updated direct dependencies, then the count (bun lists no removals on install)", () => {
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
        "",
        "12 packages installed [432.10ms]",
        "Removed: 1",
      ].join("\n"),
    );
  });

  test("add lists each request once, after the + rows (bun's tree printer)", () => {
    const text = render(
      emptySummary({
        added: [
          { name: "react", version: "18.3.1" },
          { name: "loose-envify", version: "1.4.0" },
        ],
        requested: [{ name: "react", version: "18.3.1", bins: [] }],
        installedCount: 2,
        elapsedMs: 5,
      }),
      "add",
      pc,
    );
    expect(text).toBe(
      ["", "+ loose-envify@1.4.0", "", "installed react@18.3.1", "", "2 packages installed [5.00ms]"].join("\n"),
    );
  });

  test("remove lists the removed names right above the count, failures after it", () => {
    const text = render(emptySummary({ removed: ["lodash"], removedCount: 1, failedCount: 1, elapsedMs: 3 }), "remove", pc);
    expect(text).toBe(["", "- lodash", "1 package removed [3.00ms]", "Failed to install 1 package"].join("\n"));
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

    expect(text).toContain("installed typescript@5.9.3 with binaries:\n - tsc\n - tsserver");
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
