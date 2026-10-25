/**
 * The `registry` module of bun's tests (test/packages/registry/test-registry.ts) for the conformance suite:
 * `TestRegistry` serves bun's fixture packages from tests/registry (node:http). `start()` also points
 * the default registry (setDefaultRegistry) at it.
 */
import { join } from "node:path";
import { Registry, type RegistryOptions } from "../../registry/src/registry.ts";
import { setDefaultRegistry, tempDir, type DirectoryTree } from "../harness.ts";
import { TOML, write } from "./bun.ts";
import { bunInstallTestDir, fixturePackagesDir } from "../../registry/index.ts";

export * from "../../registry/index.ts";
export { installEnv } from "../harness.ts";

export const toml = (value: object): string => TOML.stringify(value);

export type BunfigOptions = {
  saveTextLockfile?: boolean;
  /** Leave the registry out, so that the install goes to the default one. */
  npm?: boolean;
  linker?: "isolated" | "hoisted";
  globalStore?: boolean;
  publicHoistPattern?: string | string[];
  hoistPattern?: string | string[];
  hoist?: boolean;
};

/** The packages that bun's install tests share: `test/cli/install/registry/packages`. */
export const fixturePackages = fixturePackagesDir;

export class TestRegistry extends Registry {
  readonly packagesPath: string;

  constructor(options: RegistryOptions = {}) {
    const storage = options.storage ?? fixturePackages;
    super({
      ...options,
      storage,
      // The loose fixtures (bar-0.0.2.tgz, ...) are served too when the storage is bun's fixture packages.
      tarballDirs: options.tarballDirs ?? (options.storage === undefined ? [bunInstallTestDir] : []),
      access: { "@needs-auth/*": { read: "authenticated" }, ...options.access },
    });
    this.packagesPath = storage;
  }

  override start(): this {
    super.start();
    setDefaultRegistry(this.url);
    return this;
  }

  registryUrl() {
    return this.url;
  }

  /** Creates a user through the API, as `npm adduser` does. Returns a token of the user. */
  async generateUser(username: string, password: string): Promise<string> {
    const response = await fetch(`${this.url}-/user/org.couchdb.user:${encodeURIComponent(username)}`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: username, password, email: `${username}@example.com` }),
    });
    const body = await response.text();
    if (!response.ok) throw new Error(`Failed to create user ${username}: ${response.status} ${body}`);
    return JSON.parse(body).token;
  }

  /** A bunfig.toml that points to this registry with the token of a new user. */
  async authBunfig(user: string) {
    const token = await this.generateUser(user, user);
    return toml({ install: { cache: false, registry: { url: this.url, token } } });
  }

  async createTestDir(
    options: { bunfigOpts?: BunfigOptions; files?: DirectoryTree | string } = {
      bunfigOpts: { linker: "hoisted" },
      files: {},
    },
  ) {
    const packageDir = String(tempDir("registry-test-", options.files ?? {}));
    await this.writeBunfig(packageDir, options.bunfigOpts);
    return { packageDir, packageJson: join(packageDir, "package.json") };
  }

  async writeBunfig(dir: string, options: BunfigOptions = {}) {
    await write(
      join(dir, "bunfig.toml"),
      toml({
        install: {
          cache: join(dir, ".bun-cache"),
          saveTextLockfile: options.saveTextLockfile,
          registry: options.npm ? undefined : this.url,
          linker: options.linker,
          globalStore: options.globalStore,
          publicHoistPattern: options.publicHoistPattern,
          hoistPattern: options.hoistPattern,
          hoist: options.hoist,
        },
      }),
    );
  }
}
