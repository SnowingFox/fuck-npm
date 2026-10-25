import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, test } from "vitest";

const CLI_ROOT = join(import.meta.dirname, "..", "..", "..");
const read = (...parts: string[]) => readFileSync(join(CLI_ROOT, ...parts), "utf-8");

const sourceFiles = (dir: string): string[] =>
  readdirSync(dir).flatMap((entry) => {
    const path = join(dir, entry);
    return statSync(path).isDirectory() ? sourceFiles(path) : path.endsWith(".ts") ? [path] : [];
  });

describe("bundle shebang", () => {
  test.each(["src/bin/fnpm.ts", "src/bin/fnpm-lock.ts", "src/entry.ts"])(
    "%s has no shebang of its own",
    (file) => {
      // tsup's banner adds one. A second copy lands on line 2 of the bundle as
      // a syntax error that only `node dist/...` ever sees.
      expect(read(file).startsWith("#!")).toBe(false);
    },
  );

  test("tsup supplies the shebang via its banner, for both bins", () => {
    const config = read("tsup.config.ts");

    expect(config).toMatch(/banner:\s*\{\s*js:\s*"#!\/usr\/bin\/env node/);
    expect(config).toContain('fnpm: "./src/bin/fnpm.ts"');
    expect(config).toContain('"fnpm-lock": "./src/bin/fnpm-lock.ts"');
  });

  test("the entry-point guard resolves symlinks and lives in each bin file", () => {
    expect(read("src/entry.ts")).toContain("realpathSync");
    // With splitting, import.meta.url outside the entry file names a chunk.
    for (const bin of ["src/bin/fnpm.ts", "src/bin/fnpm-lock.ts"]) {
      expect(read(bin)).toContain("isEntryPoint(import.meta.url");
    }
  });
});

describe("startup path", () => {
  test("@clack/prompts is only ever imported dynamically", () => {
    for (const file of sourceFiles(join(CLI_ROOT, "src"))) {
      expect(readFileSync(file, "utf-8"), file).not.toMatch(/from\s+["']@clack\/prompts["']/);
    }
  });

  test("the help renderer is not statically imported by the dispatcher", () => {
    const runtime = read("src/cli/runtime.ts");

    expect(runtime).not.toMatch(/from\s+"\.\/help\.ts"/);
    expect(runtime).toContain('import("./help.ts")');
  });

  test("the compile cache is enabled before the app loads", () => {
    for (const bin of ["src/bin/fnpm.ts", "src/bin/fnpm-lock.ts"]) {
      const source = read(bin);
      expect(source.indexOf("enableCompileCache()")).toBeGreaterThan(-1);
      expect(source.indexOf("enableCompileCache()")).toBeLessThan(source.indexOf("await main("));
    }
  });
});

// Vitest runs src/; users run dist/. When a build is present, check the
// artifact itself (shebang count, entry guard, --version through a symlink-free path).
describe.runIf(existsSync(join(CLI_ROOT, "dist", "fnpm.js")))("built bundle", () => {
  test.each(["fnpm.js", "fnpm-lock.js"])("dist/%s has exactly one shebang and runs", (file) => {
    const bundle = read("dist", file);
    expect(bundle.startsWith("#!/usr/bin/env node\n")).toBe(true);
    expect(bundle.split("\n")[1]?.startsWith("#!")).toBe(false);

    const result = spawnSync(process.execPath, [join(CLI_ROOT, "dist", file), "--version"], {
      encoding: "utf-8",
    });
    expect(result.status).toBe(0);
    expect(result.stdout.trim()).toMatch(/^\d+\.\d+\.\d+/);
  });

  test("dist/fnpm.js --tree prints the command tree", () => {
    const result = spawnSync(process.execPath, [join(CLI_ROOT, "dist", "fnpm.js"), "--tree"], {
      encoding: "utf-8",
      env: { ...process.env, FORCE_COLOR: "0" },
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("├── install");
    expect(result.stdout).not.toContain("\u001B[");
  });
});
