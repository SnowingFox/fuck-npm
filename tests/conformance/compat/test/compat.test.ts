import fs from "node:fs";
import { join } from "../path.ts";
import { afterAll, describe, expect, test } from "vitest";
import { fnpmEnv, readLockfile, runFnpm, tempDir, tempDirWithFiles } from "../../harness.ts";
import { $, Archive, file, Glob, JSONC, serve, spawn, spawnSync, TOML, write } from "../bun.ts";
import * as bunTest from "../bun-test.ts";
import { readTarball } from "../internal-for-testing.ts";

const dirs: string[] = [];
const temp = (files = {}) => {
  const dir = tempDirWithFiles("fnpm-compat", files);
  dirs.push(dir);
  return dir;
};
afterAll(() => {
  for (const dir of dirs) fs.rmSync(dir, { recursive: true, force: true });
});

describe("spawn", () => {
  test("pipes stdout and stderr, resolves exited", async () => {
    const proc = spawn({
      cmd: [process.execPath, "-e", "process.stdout.write('out'); process.stderr.write('err'); process.exit(3)"],
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr, code] = await Promise.all([proc.stdout!.text(), proc.stderr!.text(), proc.exited]);
    expect([stdout, stderr, code]).toEqual(["out", "err", 3]);
    expect(proc.exitCode).toBe(3);
  });

  test("new Response(stdout).text() and stdin pipe", async () => {
    const proc = spawn({
      cmd: [process.execPath, "-e", "process.stdin.pipe(process.stdout)"],
      stdin: "pipe",
      stdout: "pipe",
    });
    proc.stdin!.write("hello ");
    proc.stdin!.end("world");
    expect(await new Response(proc.stdout).text()).toBe("hello world");
    expect(await proc.exited).toBe(0);
  });

  test("a missing executable exits 127 with the error on stderr", async () => {
    const proc = spawn({ cmd: ["/nonexistent/fnpm"], stdout: "pipe", stderr: "pipe" });
    const [stderr, code] = await Promise.all([proc.stderr!.text(), proc.exited]);
    expect(code).toBe(127);
    expect(stderr).toContain("ENOENT");
  });

  test("a .js script runs under node", async () => {
    const dir = temp({ "cli.js": "console.log(process.argv.slice(2).join(','))" });
    const proc = spawn({ cmd: [join(dir, "cli.js"), "a", "b"], stdout: "pipe" });
    expect(await proc.stdout!.text()).toBe("a,b\n");
  });

  test("spawnSync", () => {
    const result = spawnSync({ cmd: [process.execPath, "-e", "console.log(1)"] });
    expect(result.stdout.toString()).toBe("1\n");
    expect(result.exitCode).toBe(0);
    expect(result.success).toBe(true);
  });
});

describe("file / write", () => {
  test("write creates parents, file reads back", async () => {
    const dir = temp();
    const path = join(dir, "a", "b", "c.json");
    expect(await write(path, JSON.stringify({ x: 1 }))).toBe(7);
    const f = file(path);
    expect(await f.exists()).toBe(true);
    expect(await f.json()).toEqual({ x: 1 });
    expect(f.size).toBe(7);
    expect(await file(join(dir, "missing")).exists()).toBe(false);
  });

  test("a BunFile is a Response body", async () => {
    const dir = temp({ "x.txt": "content" });
    expect(await new Response(file(join(dir, "x.txt"))).text()).toBe("content");
  });
});

test("$ quotes interpolations and honours cwd/nothrow", async () => {
  const dir = temp();
  const name = "it's a file";
  await $`touch ${name}`.cwd(dir);
  expect(fs.existsSync(join(dir, name))).toBe(true);
  expect((await $`echo ${["a b", "c"]}`.text()).trim()).toBe("a b c");
  const failed = await $`exit 4`.nothrow();
  expect(failed.exitCode).toBe(4);
  await expect($`exit 5`.quiet()).rejects.toThrow("exit code 5");
});

test("Archive round trip and readTarball", async () => {
  const bytes = await new Archive(
    { "package/package.json": '{"name":"x"}', "package/dir/": "", [`package/${"n".repeat(120)}.js`]: "long" },
    { compress: "gzip" },
  ).bytes();
  expect([bytes[0], bytes[1]]).toEqual([0x1f, 0x8b]);
  const files = await new Archive(bytes).files();
  expect([...files.keys()].sort()).toEqual(["package/package.json", `package/${"n".repeat(120)}.js`].sort());
  expect(await files.get("package/package.json")!.text()).toBe('{"name":"x"}');
  const dir = temp();
  await Archive.write(join(dir, "x.tgz"), { "package/a.txt": "A" }, { compress: "gzip" });
  expect(readTarball(join(dir, "x.tgz")).entries).toMatchObject([{ pathname: "package/a.txt", contents: "A" }]);
});

test("serve answers fetch handlers on a synchronous port", async () => {
  const server = serve({ port: 0, fetch: req => new Response(new URL(req.url).pathname) });
  try {
    expect(server.port).toBeGreaterThan(0);
    expect(server.url.href).toBe(`http://localhost:${server.port}/`);
    expect(await (await fetch(`${server.url}hello`)).text()).toBe("/hello");
    expect(await (await fetch(`http://[::1]:${server.port}/v6`).catch(() => fetch(`http://127.0.0.1:${server.port}/v6`))).text()).toBe("/v6");
  } finally {
    await server.stop();
  }
});

test("JSONC and TOML", () => {
  expect(JSONC.parse('{\n  // c\n  "a": [1, 2,],\n  /* b */ "s": "x // not a comment",\n}')).toEqual({ a: [1, 2], s: "x // not a comment" });
  const toml = TOML.stringify({ install: { registry: "http://x/", cache: false, skip: undefined } });
  expect(TOML.parse(toml)).toEqual({ install: { registry: "http://x/", cache: false } });
});

test("Glob", () => {
  const dir = temp({ "a/node_modules/x/package.json": "{}", "b.txt": "", ".hidden/c.txt": "" });
  expect([...new Glob("**/*.txt").scanSync({ cwd: dir })].sort()).toEqual(["b.txt"]);
  expect([...new Glob("**/*.txt").scanSync({ cwd: dir, dot: true })].sort()).toEqual([".hidden/c.txt", "b.txt"]);
  expect([...new Glob("**/node_modules/*").scanSync({ cwd: dir, onlyFiles: false })]).toEqual(["a/node_modules/x"]);
});

test.skipIf(process.platform === "win32")("Glob excludes directory symlinks from file scans without following them", () => {
  const dir = temp({ "source/file.txt": "content" });
  fs.symlinkSync(join(dir, "source"), join(dir, "directory-link"), "dir");
  fs.symlinkSync(join(dir, "source/file.txt"), join(dir, "file-link"), "file");
  const glob = new Glob("**/*");
  expect([...glob.scanSync({ cwd: dir })].sort()).toEqual(["file-link", "source/file.txt"]);
  expect([...glob.scanSync({ cwd: dir, onlyFiles: false })].sort()).toEqual([
    "directory-link", "file-link", "source", "source/file.txt",
  ]);
});

describe("bun:test modifiers", () => {
  bunTest.test.if(true)("test.if(true) runs", () => {});
  bunTest.test.skipIf(true)("test.skipIf(true) is skipped", () => {
    throw new Error("must not run");
  });
  bunTest.test.todoIf(true)("test.todoIf(true) is todo", () => {
    throw new Error("must not run");
  });
  bunTest.test.failing("test.failing passes when the body throws", () => {
    throw new Error("expected");
  });
  bunTest.test.concurrent.each([1, 2])("test.concurrent.each %d", (n: number) => {
    expect(n).toBeGreaterThan(0);
  });
  bunTest.test("bun's (name, fn, options) signature", () => {}, { timeout: 1000 });
  bunTest.describe.skipIf(true)("describe.skipIf(true)", () => {
    bunTest.test("is skipped", () => {
      throw new Error("must not run");
    });
  });
});

describe("harness", () => {
  test("tempDir is disposable", () => {
    let path: string;
    {
      using dir = tempDir("disposable", { "x.txt": "x" });
      path = String(dir);
      expect(fs.readFileSync(join(path, "x.txt"), "utf8")).toBe("x");
    }
    expect(fs.existsSync(path)).toBe(false);
  });

  test("disposable paths work with path helpers and spawn cwd", async () => {
    using dir = tempDir("boxed-path", { "x.txt": "x" });
    expect(fs.readFileSync(join(dir, "x.txt"), "utf8")).toBe("x");
    const proc = spawn({ cmd: [process.execPath, "-p", "process.cwd()"], cwd: dir, stdout: "pipe" });
    expect((await proc.stdout!.text()).trim()).toBe(String(dir));
    expect(await proc.exited).toBe(0);
    const sync = spawnSync({ cmd: [process.execPath, "-p", "process.cwd()"], cwd: dir });
    expect(sync.stdout.toString().trim()).toBe(String(dir));
    expect(sync.exitCode).toBe(0);
  });

  test("fnpmEnv isolates the store and cache", () => {
    expect(fnpmEnv.FNPM_STORE_DIR).toContain("fnpm-conformance-");
    expect(fnpmEnv.FNPM_CACHE_DIR).toContain("fnpm-conformance-");
    expect(fnpmEnv.NO_COLOR).toBe("1");
    expect(Object.keys(fnpmEnv).some(key => key.startsWith("npm_"))).toBe(false);
  });

  test("runFnpm runs FNPM_EXE with fnpmEnv; readLockfile parses JSONC", async () => {
    const dir = temp({
      "fake-fnpm.js": "console.log(JSON.stringify({ args: process.argv.slice(2), store: process.env.FNPM_STORE_DIR }))",
      "fnpm.lock": '{\n  "lockfileVersion": 1,\n  "packages": {},\n}\n',
    });
    const previous = process.env.FNPM_EXE;
    process.env.FNPM_EXE = join(dir, "fake-fnpm.js");
    try {
      const { stdout, exitCode } = await runFnpm(["install", "--frozen-lockfile"], { cwd: dir });
      expect(exitCode).toBe(0);
      expect(JSON.parse(stdout)).toEqual({ args: ["install", "--frozen-lockfile"], store: fnpmEnv.FNPM_STORE_DIR });
    } finally {
      if (previous === undefined) delete process.env.FNPM_EXE;
      else process.env.FNPM_EXE = previous;
    }
    expect(await readLockfile(dir)).toEqual({ lockfileVersion: 1, packages: {} });
  });
});
