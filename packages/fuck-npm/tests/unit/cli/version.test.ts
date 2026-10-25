import { describe, expect, test } from "vitest";
import { getCliVersion } from "@/cli/version.ts";
import { testBed } from "../harness.ts";

describe("getCliVersion", () => {
  test("reports the real package version, never the 0.0.0 fallback", () => {
    // Catches the rename trap: the manifest is matched by name, so renaming
    // the package without updating PACKAGE_NAME silently reports 0.0.0.
    expect(getCliVersion()).not.toBe("0.0.0");
    expect(getCliVersion()).toMatch(/^\d+\.\d+\.\d+/);
  });

  test("matches the version in package.json", async () => {
    const manifest = (await import("../../../package.json", {
      with: { type: "json" },
    })) as { default: { version: string } };

    expect(getCliVersion()).toBe(manifest.default.version);
  });
});

describe("--version", () => {
  test.each([["--version"], ["-v"]])("%s prints the version and exits 0", async (flag) => {
    const bed = testBed();

    const code = await bed.run([flag]);

    expect(code).toBe(0);
    expect(bed.stdout.text().trim()).toBe(getCliVersion());
    expect(bed.coreLoads()).toBe(0);
  });

  test("fnpm-lock -v prints the same version", async () => {
    const bed = testBed();

    expect(await bed.run(["-v"], "fnpm-lock")).toBe(0);
    expect(bed.stdout.text().trim()).toBe(getCliVersion());
  });
});
