import { describe, expect, test } from "vitest";
import { parseFlags } from "@/cli/parse-flags.ts";
import { indexRawFlagNames, indexRawFlags, rawFlagLookup } from "@/cli/raw-flags.ts";
import { flag } from "@/cli/registry.ts";

const specs = [
  flag("-d, --dev", "dev"),
  flag("-E, --exact", "exact"),
  flag("-F, --filter [pattern]", "filter", { repeatable: true }),
  flag("--otp [code]", "otp"),
  flag("--catalog [name]", "catalog", { optionalValue: true }),
  flag("--no-save", "no save"),
  flag("--save", "save"),
];

describe("parseFlags", () => {
  test("keeps value strings exactly as typed (mri would coerce them)", () => {
    const { values } = parseFlags(["--otp", "007", "-F", "1e3"], specs);

    expect(values.otp).toBe("007");
    expect(values.filter).toEqual(["1e3"]);
  });

  test("--flag=value, -Fvalue and -F=value", () => {
    const { values } = parseFlags(["--otp=0x1f", "-Fapp", "-F=web"], specs);

    expect(values.otp).toBe("0x1f");
    expect(values.filter).toEqual(["app", "web"]);
  });

  test("short switch clusters", () => {
    expect(parseFlags(["-dE"], specs).values).toEqual({ dev: true, exact: true });
  });

  test("an optional-value flag only takes its value with =", () => {
    expect(parseFlags(["--catalog", "react"], specs)).toEqual({
      values: { catalog: true },
      positionals: ["react"],
    });
    expect(parseFlags(["--catalog=r19"], specs).values.catalog).toBe("r19");
  });

  test("--no-<boolean> negates; an explicit --no-x spec wins", () => {
    expect(parseFlags(["--no-dev"], specs).values.dev).toBe(false);
    expect(parseFlags(["--no-save"], specs).values).toEqual({ noSave: true });
  });

  test("booleans accept =true/=false and reject other values", () => {
    expect(parseFlags(["--dev=false"], specs).values.dev).toBe(false);
    expect(() => parseFlags(["--dev=maybe"], specs)).toThrow("does not take a value");
  });

  test("a value flag with no value is an error naming the flag", () => {
    expect(() => parseFlags(["--otp"], specs)).toThrow("`--otp` requires a value.");
    expect(() => parseFlags(["-F", "--dev"], specs)).toThrow("`-F` requires a value.");
  });

  test("positionals keep their order around flags", () => {
    expect(parseFlags(["react", "-d", "zod"], specs).positionals).toEqual(["react", "zod"]);
  });
});

describe("raw-flags", () => {
  const argv = ["node", "fnpm", "add", "--otp", "8675309123456789012", "--tag=007", "--dev", "--", "--otp", "x"];

  test("recovers values verbatim and stops at --", () => {
    const index = indexRawFlags(argv);

    expect(index.get("otp")).toEqual(["8675309123456789012"]);
    expect(index.get("tag")).toEqual(["007"]);
    expect(rawFlagLookup(argv)("otp")).toBe("8675309123456789012");
  });

  test("records presence of booleans", () => {
    expect(indexRawFlagNames(argv).has("dev")).toBe(true);
    expect(indexRawFlagNames(argv).has("x")).toBe(false);
  });
});
