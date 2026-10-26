import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createServer, type Server } from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { afterEach, describe, expect, test } from "vitest";
import { installNative } from "../../../scripts/install-native.js";
import { extractFiles, matchesIntegrity, postinstall, skipReason } from "../../../scripts/postinstall.js";

/**
 * The published bin shims (`bin/fnpm`, `lib/shim.cjs`) and the postinstall
 * swap (`scripts/postinstall.js`), run for real against a package laid out in
 * a temp `node_modules`, with shell scripts standing in for the native
 * executables and a local HTTP server standing in for the registry.
 */

const PACKAGE = fileURLToPath(new URL("../../../", import.meta.url));
const require = createRequire(import.meta.url);
const native = require("../../../lib/native.cjs") as {
  platformArchAbi(): string | null;
  executableName(bin: string): string;
};
const ABI = native.platformArchAbi();
const VERSION = (JSON.parse(readFileSync(join(PACKAGE, "package.json"), "utf8")) as { version: string }).version;
const unix = process.platform !== "win32" && ABI !== null;

const roots: string[] = [];
const servers: Server[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
  for (const server of servers.splice(0)) server.close();
});

type Layout = { root: string; pkg: string; platformDir: string };

/** `<tmp>/node_modules/fuck-npm` with the real shim, lib and postinstall, and a fake dist. */
function layout(): Layout {
  const root = mkdtempSync(join(tmpdir(), "fnpm-shim-"));
  roots.push(root);
  const pkg = join(root, "node_modules", "fuck-npm");
  for (const file of ["package.json", "bin/fnpm", "bin/fnpm-lock", "bin/fnpmx", "bin/fnpx", "bin/package.json", "lib/native.cjs", "lib/shim.cjs", "lib/archive.cjs", "scripts/postinstall.js", "scripts/install-native.js"]) {
    mkdirSync(dirname(join(pkg, file)), { recursive: true });
    copyFileSync(join(PACKAGE, file), join(pkg, file));
  }
  chmodSync(join(pkg, "bin/fnpm"), 0o755);
  chmodSync(join(pkg, "bin/fnpm-lock"), 0o755);
  mkdirSync(join(pkg, "dist"));
  for (const bin of ["fnpm", "fnpm-lock"]) {
    writeFileSync(
      join(pkg, "dist", `${bin}.js`),
      `process.stdout.write(JSON.stringify({ js: ${JSON.stringify(bin)}, entry: process.argv[1], args: process.argv.slice(2) }) + "\\n");\nprocess.exitCode = 5;\n`,
    );
  }
  return { root, pkg, platformDir: join(root, "node_modules", "@fuck-npm", `binding-${ABI}`) };
}

/** A shell script answering `--version` with `version` and otherwise printing its argv. */
function fakeExecutable(bin: string, version: string, body = "exit 3"): string {
  return [
    "#!/bin/sh",
    `if [ "$1" = "--version" ]; then echo ${version}; exit 0; fi`,
    `printf '%s\\n' "native:${bin}" "$@" "entry=$FNPM_JS_ENTRY" "lock=$FNPM_LOCK_JS_ENTRY"`,
    body,
    "",
  ].join("\n");
}

function installPlatformPackage(dir: string, version = VERSION, body?: string): void {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: `@fuck-npm/binding-${ABI}`, version }));
  for (const bin of ["fnpm", "fnpm-lock"]) {
    writeFileSync(join(dir, bin), fakeExecutable(bin, version, body));
    chmodSync(join(dir, bin), 0o755);
  }
}

const runShim = (pkg: string, bin: string, args: string[], env: NodeJS.ProcessEnv = {}) =>
  spawnSync(process.execPath, [join(pkg, "bin", bin), ...args], {
    encoding: "utf8",
    env: { PATH: process.env.PATH, ...env },
  });

describe.runIf(unix)("bin shim", () => {
  test("runs the TS CLI in process when there is no native executable", () => {
    const { pkg } = layout();
    const result = runShim(pkg, "fnpm", ["outdated", "--json", "a b"]);
    expect(result.status).toBe(5);
    expect(JSON.parse(result.stdout)).toEqual({
      js: "fnpm",
      entry: join(pkg, "dist", "fnpm.js"),
      args: ["outdated", "--json", "a b"],
    });
  });

  test("fnpm-lock runs its own bundle", () => {
    const { pkg } = layout();
    const result = runShim(pkg, "fnpm-lock", ["migrate"]);
    expect(JSON.parse(result.stdout)).toMatchObject({ js: "fnpm-lock", args: ["migrate"] });
  });

  test("runs the native executable as a child, pointing it at the bundle", () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    const result = runShim(pkg, "fnpm", ["install", "--frozen-lockfile", ""]);
    expect(result.status).toBe(3);
    expect(result.stdout.split("\n")).toEqual([
      "native:fnpm",
      "install",
      "--frozen-lockfile",
      "",
      `entry=${join(pkg, "dist", "fnpm.js")}`,
      `lock=${join(pkg, "dist", "fnpm-lock.js")}`,
      "",
    ]);
  });

  test.each(["fnpmx", "fnpx"])("%s prepends x for the JS fallback and native child", (alias) => {
    const { pkg, platformDir } = layout();
    const args = ["--package", "tool", "tool-bin", "--", "a b"];
    const fallback = runShim(pkg, alias, args);
    expect(fallback.status).toBe(5);
    expect(JSON.parse(fallback.stdout)).toMatchObject({ js: "fnpm", args: ["x", ...args] });
    installPlatformPackage(platformDir);
    const result = runShim(pkg, alias, args);
    expect(result.status).toBe(3);
    expect(result.stdout.split("\n").slice(0, args.length + 2)).toEqual(["native:fnpm", "x", ...args]);
  });

  test("keeps an explicit FNPM_JS_ENTRY", () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    const result = runShim(pkg, "fnpm", [], { FNPM_JS_ENTRY: "/custom/fnpm.js" });
    expect(result.stdout).toContain("entry=/custom/fnpm.js\n");
  });

  test("dies by the signal that killed the child", () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir, VERSION, "kill -TERM $$");
    const result = runShim(pkg, "fnpm", ["install"]);
    expect(result.signal).toBe("SIGTERM");
  });

  test("forwards SIGTERM to the child and exits with its status", async () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir, VERSION, `trap 'echo got-term; exit 9' TERM\necho ready\nwhile :; do sleep 0.05; done`);
    const child = spawn(process.execPath, [join(pkg, "bin", "fnpm"), "install"], { stdio: ["ignore", "pipe", "inherit"] });
    let out = "";
    const exited = new Promise<number | null>((resolve) => child.on("exit", (code) => resolve(code)));
    await new Promise<void>((resolve) =>
      child.stdout.on("data", (chunk: Buffer) => {
        out += chunk.toString();
        if (out.includes("ready")) resolve();
      }),
    );
    child.kill("SIGTERM");
    expect(await exited).toBe(9);
    expect(out).toContain("got-term");
  });

  test("finds the platform package through @fuck-npm/binding in an isolated layout", () => {
    const { root, pkg } = layout();
    // pnpm-style: the platform package is only visible to @fuck-npm/binding, its dependent.
    const store = join(root, "node_modules", ".store", "binding", "node_modules", "@fuck-npm");
    mkdirSync(join(store, "binding"), { recursive: true });
    writeFileSync(join(store, "binding", "package.json"), JSON.stringify({ name: "@fuck-npm/binding", version: VERSION }));
    installPlatformPackage(join(store, `binding-${ABI}`));
    mkdirSync(join(pkg, "node_modules", "@fuck-npm"), { recursive: true });
    symlinkSync(join(store, "binding"), join(pkg, "node_modules", "@fuck-npm", "binding"));
    const result = runShim(pkg, "fnpm", ["install"]);
    expect(result.stdout.startsWith("native:fnpm\ninstall\n")).toBe(true);
    expect(result.status).toBe(3);
  });

  test("falls back to the TS CLI when the executable cannot run", () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    chmodSync(join(platformDir, "fnpm"), 0o644);
    const result = runShim(pkg, "fnpm", ["why", "x"]);
    expect(JSON.parse(result.stdout)).toMatchObject({ js: "fnpm", args: ["why", "x"] });
  });
});

describe("postinstall skip rules", () => {
  const root = join("/x", "node_modules", "fuck-npm");
  test.each([
    [{ env: { FNPM_SKIP_NATIVE_SWAP: "1" } }, "FNPM_SKIP_NATIVE_SWAP=1"],
    [{ platform: "win32" as const }, "Windows"],
    [{ root: "/repo/packages/fuck-npm" }, "development checkout"],
    [{ env: { npm_config_user_agent: "pnpm/10.0.0 npm/? node/v22.16.0 linux x64" } }, "pnpm"],
    [{ env: { npm_config_user_agent: "yarn/4.5.0 npm/? node/v22.16.0 linux x64" } }, "yarn berry"],
  ])("%j", (options, reason) => {
    expect(skipReason({ root, platform: "linux", env: {}, ...options })).toContain(reason);
  });

  test.each([
    "npm/10.9.2 node/v22.16.0 linux x64 workspaces/false",
    "yarn/1.22.22 npm/? node/v22.16.0 linux x64",
    "bun/1.3.0 npm/? node/v24.3.0 linux x64",
    "",
  ])("swaps under %j", (agent) => {
    expect(skipReason({ root, platform: "linux", env: { npm_config_user_agent: agent } })).toBeNull();
  });
});

const NPM_ENV = { npm_config_user_agent: "npm/10.9.2 node/v22.16.0 linux x64" };

describe.runIf(unix)("postinstall swap", () => {
  test("hardlinks the platform executables over the shims", async () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    expect(await postinstall({ root: pkg, env: NPM_ENV, abi: ABI })).toBe("swapped");
    for (const bin of ["fnpm", "fnpm-lock"]) {
      expect(statSync(join(pkg, "bin", bin)).ino).toBe(statSync(join(platformDir, bin)).ino);
      expect(statSync(join(pkg, "bin", bin)).mode & 0o111).not.toBe(0);
    }
    for (const alias of ["fnpmx", "fnpx"]) {
      expect(statSync(join(pkg, "bin", alias)).ino).toBe(statSync(join(platformDir, "fnpm")).ino);
    }
    // .bin/fnpm -> ../fuck-npm/bin/fnpm now starts the native executable directly.
    const dotBin = join(pkg, "..", ".bin");
    mkdirSync(dotBin);
    symlinkSync("../fuck-npm/bin/fnpm", join(dotBin, "fnpm"));
    const result = spawnSync(join(dotBin, "fnpm"), ["add", "x"], { encoding: "utf8" });
    expect(result.status).toBe(3);
    expect(result.stdout.startsWith("native:fnpm\nadd\nx\n")).toBe(true);
  });

  test("runs as a script", () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    const result = spawnSync(process.execPath, ["scripts/postinstall.js"], {
      cwd: pkg,
      encoding: "utf8",
      env: { PATH: process.env.PATH, ...NPM_ENV, FNPM_DEBUG_POSTINSTALL: "1" },
    });
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("fnpm postinstall: swapped");
    expect(statSync(join(pkg, "bin", "fnpm")).ino).toBe(statSync(join(platformDir, "fnpm")).ino);
  });

  test("keeps the shim when the executable reports another version", async () => {
    const { pkg, platformDir } = layout();
    installPlatformPackage(platformDir, "9.9.9");
    const shim = readFileSync(join(pkg, "bin", "fnpm"), "utf8");
    const status = await postinstall({ root: pkg, env: NPM_ENV, abi: ABI, npm: "/nonexistent/npm" });
    expect(status).toMatch(/^kept shim: .*reports 9\.9\.9, expected /);
    expect(readFileSync(join(pkg, "bin", "fnpm"), "utf8")).toBe(shim);
  });

  test("does not reinstall a platform package that ships no executables", async () => {
    const { root, pkg, platformDir } = layout();
    installPlatformPackage(platformDir);
    for (const bin of ["fnpm", "fnpm-lock"]) rmSync(join(platformDir, bin));
    const marker = join(root, "npm-ran");
    const npm = join(root, "fake-npm");
    writeFileSync(npm, `#!/bin/sh\ntouch ${JSON.stringify(marker)}\nexit 1\n`);
    chmodSync(npm, 0o755);
    let fetched = 0;
    const fetchImpl = (async () => {
      fetched += 1;
      throw new Error("no network in this test");
    }) as unknown as typeof fetch;
    const status = await postinstall({ root: pkg, env: NPM_ENV, abi: ABI, npm, fetchImpl });
    expect(status).toMatch(/^kept shim: /);
    expect(fetched).toBe(0);
    expect(() => statSync(marker)).toThrow();
  });

  test("swaps from a platform package that only @fuck-npm/binding can see", async () => {
    const { root, pkg } = layout();
    const store = join(root, "node_modules", ".store", "binding", "node_modules", "@fuck-npm");
    mkdirSync(join(store, "binding"), { recursive: true });
    writeFileSync(join(store, "binding", "package.json"), JSON.stringify({ name: "@fuck-npm/binding", version: VERSION }));
    installPlatformPackage(join(store, `binding-${ABI}`));
    mkdirSync(join(pkg, "node_modules", "@fuck-npm"), { recursive: true });
    symlinkSync(join(store, "binding"), join(pkg, "node_modules", "@fuck-npm", "binding"));
    expect(await postinstall({ root: pkg, env: NPM_ENV, abi: ABI, npm: "/nonexistent/npm" })).toBe("swapped");
    expect(statSync(join(pkg, "bin", "fnpm")).ino).toBe(statSync(join(store, `binding-${ABI}`, "fnpm")).ino);
  });

  test("never throws, and leaves a skipped install alone", async () => {
    const { pkg } = layout();
    const status = await postinstall({ root: pkg, env: { ...NPM_ENV, FNPM_SKIP_NATIVE_SWAP: "1" }, abi: ABI });
    expect(status).toBe("skipped: FNPM_SKIP_NATIVE_SWAP=1");
    expect(await postinstall({ root: pkg, env: NPM_ENV, abi: null })).toMatch(/^kept shim: no prebuilt/);
  });
});

/** A ustar archive (`package/<name>` entries), gzipped like an npm tarball. */
function tarball(entries: [name: string, body: string, mode?: number][]): Buffer {
  const blocks: Buffer[] = [];
  for (const [name, body, mode = 0o644] of entries) {
    const header = Buffer.alloc(512);
    const put = (value: string, offset: number, length: number) => header.write(value, offset, length, "utf8");
    put(name, 0, 100);
    put(`${mode.toString(8).padStart(7, "0")}\0`, 100, 8);
    put("0000000\0", 108, 8);
    put("0000000\0", 116, 8);
    put(`${Buffer.byteLength(body).toString(8).padStart(11, "0")}\0`, 124, 12);
    put("00000000000\0", 136, 12);
    put("        ", 148, 8);
    put("0", 156, 1);
    put("ustar\0", 257, 6);
    put("00", 263, 2);
    let sum = 0;
    for (const byte of header) sum += byte;
    put(`${sum.toString(8).padStart(6, "0")}\0 `, 148, 8);
    const data = Buffer.alloc(Math.ceil(Buffer.byteLength(body) / 512) * 512);
    data.write(body);
    blocks.push(header, data);
  }
  blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks));
}

const sri = (bytes: Buffer) => `sha512-${createHash("sha512").update(bytes).digest("base64")}`;

describe("tarball handling", () => {
  test("extracts only the wanted files and never escapes the destination", () => {
    const root = mkdtempSync(join(tmpdir(), "fnpm-tar-"));
    roots.push(root);
    const dest = join(root, "out");
    const tgz = tarball([
      ["package/package.json", "{}"],
      ["package/fnpm", "binary"],
      ["package/../evil", "nope"],
      ["package/other", "skip"],
    ]);
    expect(extractFiles(tgz, dest, ["package.json", "fnpm", "../evil"])).toEqual(["package.json", "fnpm"]);
    expect(readFileSync(join(dest, "fnpm"), "utf8")).toBe("binary");
    expect(() => statSync(join(root, "evil"))).toThrow();
  });

  test("integrity", () => {
    const bytes = Buffer.from("hello");
    expect(matchesIntegrity(bytes, sri(bytes))).toBe(true);
    expect(matchesIntegrity(bytes, `sha1-bogus ${sri(bytes)}`)).toBe(true);
    expect(matchesIntegrity(Buffer.from("other"), sri(bytes))).toBe(false);
    expect(matchesIntegrity(bytes, "")).toBe(false);
    expect(matchesIntegrity(bytes, "md5-xyz")).toBe(false);
  });
});

/** A registry serving one version of the platform package. */
async function registry(tgz: Buffer, integrity: string): Promise<string> {
  const name = `@fuck-npm/binding-${ABI}`;
  const server = createServer((request, response) => {
    const base = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
    const url = decodeURIComponent(request.url ?? "");
    if (url === `/${name}`) {
      response.setHeader("content-type", "application/json");
      response.end(
        JSON.stringify({
          name,
          "dist-tags": { latest: VERSION },
          versions: {
            [VERSION]: {
              name,
              version: VERSION,
              dist: { tarball: `${base}/${name}/-/binding-${ABI}-${VERSION}.tgz`, integrity },
            },
          },
        }),
      );
      return;
    }
    if (url.endsWith(".tgz")) {
      response.end(tgz);
      return;
    }
    response.statusCode = 404;
    response.end("{}");
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return `http://127.0.0.1:${(server.address() as { port: number }).port}`;
}

function platformTarball(): Buffer {
  return tarball([
    ["package/package.json", JSON.stringify({ name: `@fuck-npm/binding-${ABI}`, version: VERSION })],
    [`package/fnpm.${ABI}.node`, "native-addon"],
    ["package/fnpm", fakeExecutable("fnpm", VERSION), 0o755],
    ["package/fnpm-lock", fakeExecutable("fnpm-lock", VERSION), 0o755],
  ]);
}

describe.runIf(unix)("postinstall without the platform package", () => {
  test("downloads it from the registry when npm is unavailable", async () => {
    const { pkg } = layout();
    const tgz = platformTarball();
    const url = await registry(tgz, sri(tgz));
    const env = { ...NPM_ENV, npm_config_registry: url };
    expect(await postinstall({ root: pkg, env, abi: ABI, npm: "/nonexistent/npm" })).toBe("swapped");
    const result = spawnSync(join(pkg, "bin", "fnpm"), ["ci"], { encoding: "utf8" });
    expect(result.stdout.startsWith("native:fnpm\nci\n")).toBe(true);
    const addon = join(pkg, "node_modules", "@fuck-npm", `binding-${ABI}`, `fnpm.${ABI}.node`);
    expect(readFileSync(addon, "utf8")).toBe("native-addon");
  });

  test("places the fallback beside the binding so both loaders can resolve it", async () => {
    const { root, pkg } = layout();
    const binding = join(root, "node_modules", "@fuck-npm", "binding");
    mkdirSync(binding, { recursive: true });
    writeFileSync(join(binding, "package.json"), JSON.stringify({ name: "@fuck-npm/binding", version: VERSION }));
    const tgz = platformTarball();
    const url = await registry(tgz, sri(tgz));
    const env = { ...NPM_ENV, npm_config_registry: url };
    expect(await postinstall({ root: pkg, env, abi: ABI, npm: "/nonexistent/npm" })).toBe("swapped");
    const requireBinding = createRequire(join(binding, "index.js"));
    const platform = requireBinding.resolve(`@fuck-npm/binding-${ABI}/package.json`);
    expect(existsSync(join(dirname(platform), `fnpm.${ABI}.node`))).toBe(true);
  });

  test("refuses a tarball that fails the integrity check", async () => {
    const { pkg } = layout();
    const tgz = platformTarball();
    const url = await registry(tgz, sri(Buffer.from("something else")));
    const env = { ...NPM_ENV, npm_config_registry: url };
    const status = await postinstall({ root: pkg, env, abi: ABI, npm: "/nonexistent/npm" });
    expect(status).toMatch(/^kept shim: /);
    expect(readFileSync(join(pkg, "bin", "fnpm"), "utf8")).toContain("lib/shim.cjs");
  });

  test("installs it with npm first", async () => {
    const { root, pkg } = layout();
    const tgz = platformTarball();
    const url = await registry(tgz, sri(tgz));
    // A child process: npm talks to the registry served from this event loop.
    const child = spawn(process.execPath, ["scripts/postinstall.js"], {
      cwd: pkg,
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        ...NPM_ENV,
        FNPM_DEBUG_POSTINSTALL: "1",
        npm_config_registry: url,
        npm_config_cache: join(root, "npm-cache"),
        npm_config_userconfig: join(root, "npmrc"),
        npm_config_proxy: "",
        npm_config_https_proxy: "",
        NO_PROXY: "127.0.0.1,localhost",
        no_proxy: "127.0.0.1,localhost",
      },
    });
    let stderr = "";
    child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
    const code = await new Promise<number | null>((resolve) => child.on("exit", resolve));
    expect(code).toBe(0);
    expect(stderr).toContain("fnpm postinstall: swapped");
    expect(stderr).not.toContain("downloading it");
    // npm put the package under fuck-npm's own node_modules.
    const nested = join(pkg, "node_modules", "@fuck-npm", `binding-${ABI}`, native.executableName("fnpm"));
    expect(statSync(join(pkg, "bin", "fnpm")).ino).toBe(statSync(nested).ino);
  }, 60_000);
});


describe.runIf(unix)("single-package native download", () => {
  function archiveFixture({ version = VERSION, incomplete = false } = {}) {
    const { pkg } = layout();
    const entries: [string, string][] = [
      ["fnpm", fakeExecutable("fnpm", version)],
      ["fnpm-lock", fakeExecutable("fnpm-lock", version)],
      [`fnpm.${ABI}.node`, "fixture addon"],
    ];
    const archive = tarball(incomplete ? entries.slice(0, 2) : entries);
    const files = entries.map(([name]) => name);
    writeFileSync(join(pkg, "lib/native-release.json"), JSON.stringify({
      version: VERSION,
      assets: { [ABI!]: { url: "https://example.com/native.tar.gz", integrity: sri(archive), files } },
    }));
    return { pkg, archive };
  }

  test("downloads this platform, validates it and reuses complete native files", async () => {
    const { pkg, archive } = archiveFixture();
    let requests = 0;
    const fetchImpl = (async () => { requests++; return new Response(archive); }) as typeof fetch;
    const dir = await installNative({ root: pkg, abi: ABI, fetchImpl });
    expect(dir).toBe(join(pkg, "native", ABI!));
    expect(requests).toBe(1);
    expect(readFileSync(join(dir, `fnpm.${ABI}.node`), "utf8")).toBe("fixture addon");
    expect(await installNative({ root: pkg, abi: ABI, fetchImpl })).toBe(dir);
    expect(requests).toBe(1);
    expect(await postinstall({ root: pkg, env: NPM_ENV, abi: ABI })).toBe("swapped");
  });

  test("rejects modified downloads without installing an executable", async () => {
    const { pkg } = archiveFixture();
    await expect(installNative({ root: pkg, abi: ABI, fetchImpl: (async () => new Response("modified")) as typeof fetch })).rejects.toThrow("integrity check failed");
    expect(existsSync(join(pkg, "native", ABI!, "fnpm"))).toBe(false);
  });

  test.each([
    [{ incomplete: true }, "incomplete"],
    [{ version: "999.0.0" }, "version does not match"],
  ] as const)("preserves the previous directory when validation fails: %j", async (options, message) => {
    const { pkg, archive } = archiveFixture(options);
    const dir = join(pkg, "native", ABI!);
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "previous"), "untouched");
    await expect(installNative({ root: pkg, abi: ABI, fetchImpl: (async () => new Response(archive)) as typeof fetch })).rejects.toThrow(message);
    expect(readFileSync(join(dir, "previous"), "utf8")).toBe("untouched");
    expect(existsSync(join(dir, "fnpm"))).toBe(false);
  });

  test.each([
    ["pnpm/10.0.0", "pnpm", process.platform],
    ["npm/11.0.0", "Windows", "win32"],
  ] as const)("downloads native files before retaining the %s shim", async (agent, reason, platform) => {
    const { pkg, archive } = archiveFixture();
    const shim = readFileSync(join(pkg, "bin/fnpm"));
    const status = await postinstall({ root: pkg, abi: ABI, platform, env: { npm_config_user_agent: agent }, fetchImpl: (async () => new Response(archive)) as typeof fetch });
    expect(status).toContain(reason);
    expect(readFileSync(join(pkg, "bin/fnpm"))).toEqual(shim);
    expect(runShim(pkg, "fnpm", ["--version"]).stdout.trim()).toBe(VERSION);
  });
});
