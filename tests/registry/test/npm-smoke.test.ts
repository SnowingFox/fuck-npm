import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { start, type RunningRegistry } from "../index.ts";

const here = dirname(fileURLToPath(import.meta.url));
const hasNpm = spawnSync("npm", ["--version"]).status === 0;

let registry: RunningRegistry;
let dir: string;

beforeAll(async () => {
  registry = await start({ port: 0 });
  dir = mkdtempSync(join(realpathSync(tmpdir()), "fnpm-npm-smoke-"));
});
afterAll(async () => {
  await registry.stop();
  rmSync(dir, { recursive: true, force: true });
});

function npm(args: string[], cwd: string) {
  const env: NodeJS.ProcessEnv = { ...process.env, npm_config_cache: join(cwd, ".npm-cache"), npm_config_userconfig: join(cwd, ".npmrc") };
  for (const key of Object.keys(env)) if (key.startsWith("npm_config_") && !["npm_config_cache", "npm_config_userconfig"].includes(key)) delete env[key];
  delete env.NODE_OPTIONS;
  // Async: the registry runs in this process, so a sync spawn would block it.
  return new Promise<{ status: number | null; stdout: string; stderr: string }>(resolve => {
    const child = spawn("npm", args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", chunk => (stdout += chunk));
    child.stderr.on("data", chunk => (stderr += chunk));
    child.on("close", status => resolve({ status, stdout, stderr }));
  });
}

describe.runIf(hasNpm)("npm against the harness", () => {
  test("npm install of fixtures: plain, transitive, scoped, loose tgz, and auth-required", async () => {
    const token = registry.createUser("npm-user");
    const host = new URL(registry.url).host;
    writeFileSync(join(dir, ".npmrc"), `registry=${registry.url}\n//${host}/:_authToken=${token}\n`);
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({
        name: "smoke",
        version: "1.0.0",
        dependencies: {
          "no-deps": "^1.0.0",
          "a-dep-b": "1.0.0",
          "@scoped/no-deps": "1.0.0",
          "bar": "0.0.2",
          "@needs-auth/test-pkg": "1.0.0",
        },
      }),
    );
    const result = await npm(["install", "--registry", registry.url, "--no-audit", "--no-fund", "--ignore-scripts"], dir);
    expect(result.stderr).not.toContain("ERR!");
    expect(result.status).toBe(0);

    const version = (name: string) => JSON.parse(readFileSync(join(dir, "node_modules", name, "package.json"), "utf8")).version;
    expect(version("no-deps")).toBe("1.1.0");
    expect(version("a-dep-b")).toBe("1.0.0");
    expect(version("b-dep-a")).toBe("1.0.0");
    expect(version("@scoped/no-deps")).toBe("1.0.0");
    expect(version("bar")).toBe("0.0.2");
    expect(version("@needs-auth/test-pkg")).toBe("1.0.0");

    const lock = JSON.parse(readFileSync(join(dir, "package-lock.json"), "utf8"));
    expect(lock.packages["node_modules/bar"].resolved).toBe(`${registry.url}bar/-/bar-0.0.2.tgz`);
    expect(lock.packages["node_modules/bar"].integrity).toMatch(/^sha512-/);

    const fetched = registry.requests.filter(r => r.status === 200).map(r => r.path);
    expect(fetched).toEqual(expect.arrayContaining(["/no-deps", "/@scoped%2fno-deps", "/bar/-/bar-0.0.2.tgz"]));
  });

  test("npm install retries through injected 500s", async () => {
    const retryDir = mkdtempSync(join(realpathSync(tmpdir()), "fnpm-npm-retry-"));
    try {
      writeFileSync(join(retryDir, ".npmrc"), `registry=${registry.url}\nfetch-retry-mintimeout=10\nfetch-retry-maxtimeout=50\n`);
      writeFileSync(join(retryDir, "package.json"), JSON.stringify({ name: "retry", version: "1.0.0", dependencies: { "7-no-deps": "*" } }));
      registry.addFault({ path: "/7-no-deps", status: 500, times: 2 });
      const result = await npm(["install", "--no-audit", "--no-fund", "--ignore-scripts"], retryDir);
      expect(result.status).toBe(0);
      const statuses = registry.requests.filter(r => r.path === "/7-no-deps").map(r => r.status);
      expect(statuses).toEqual([500, 500, 200]);
    } finally {
      registry.clearFaults();
      rmSync(retryDir, { recursive: true, force: true });
    }
  });
});

test("serve.ts CLI starts, answers and stops on SIGTERM", async () => {
  const child = spawn(process.execPath, [join(here, "..", "serve.ts"), "--port", "0", "--quiet"], { stdio: ["ignore", "pipe", "pipe"] });
  try {
    const url = await new Promise<string>((resolve, reject) => {
      let out = "";
      child.stdout.on("data", chunk => {
        out += chunk;
        const match = out.match(/listening on (http:\/\/\S+\/)/);
        if (match) resolve(match[1]);
      });
      child.once("exit", code => reject(new Error(`serve.ts exited with ${code}`)));
    });
    const doc = await (await fetch(`${url}no-deps`)).json();
    expect(doc.name).toBe("no-deps");
  } finally {
    child.kill("SIGTERM");
  }
  const code = await new Promise(resolve => child.once("exit", resolve));
  expect(code).toBe(0);
});
