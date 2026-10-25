import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { buildSpec, renderSpec, SPEC_PATH } from "../../../scripts/cli-spec.ts";

describe("crates/fnpm-cli/cli-spec.json", () => {
  test("is up to date with the command tree (run `npm run build:spec`)", () => {
    expect(readFileSync(SPEC_PATH, "utf8")).toBe(renderSpec());
  });

  test("carries what the native launcher dispatches on", () => {
    const spec = buildSpec();
    const names = spec.bins.fnpm?.root.children?.map((child) => child.name) ?? [];
    for (const native of ["install", "ci", "add", "remove", "update"]) {
      expect(names).toContain(native);
    }
    const install = spec.bins.fnpm?.root.children?.find((child) => child.name === "install");
    expect(install?.aliases).toContain("i");
    expect(install?.foreignFlags).toBe("install");
    const filter = install?.flags?.map((index) => spec.flags[index]).find((flag) => flag?.long === "filter");
    expect(filter).toMatchObject({ short: "F", takesValue: true, repeatable: true });
    expect(spec.foreignFlags.rewrites).toContainEqual(["--save-dev", "--dev"]);
    expect(spec.foreignFlags.rewrites).toContainEqual(["--no-audit", null]);
    expect(spec.version).toMatch(/^\d+\.\d+\.\d+/);
  });
});
