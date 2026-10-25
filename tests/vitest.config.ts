import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

const here = (path: string) => fileURLToPath(new URL(path, import.meta.url));

/**
 * Two projects:
 * - harness: unit tests of the registry harness and the compat layer. These pass today.
 * - conformance: bun's install tests ported to fnpm. They define the target and are expected to fail until the
 *   fnpm CLI implements the behaviour.
 */
export default defineConfig({
  test: {
    projects: [
      {
        test: {
          name: "harness",
          root: here("."),
          include: ["registry/test/**/*.test.ts", "conformance/compat/test/**/*.test.ts"],
          testTimeout: 30_000,
        },
      },
      {
        resolve: {
          // Safety net for bun-style imports that the porter did not rewrite.
          alias: [
            { find: /^bun:test$/, replacement: here("conformance/compat/bun-test.ts") },
            { find: /^bun:internal-for-testing$/, replacement: here("conformance/compat/internal-for-testing.ts") },
            { find: /^bun$/, replacement: here("conformance/compat/bun.ts") },
            { find: /^harness$/, replacement: here("conformance/harness.ts") },
            { find: /^registry$/, replacement: here("conformance/compat/registry.ts") },
            // Fixtures imported from bun's checkout, which FNPM_BUN_DIR names when the submodule is not checked out.
            ...(process.env.FNPM_BUN_DIR
              ? [{ find: /^(?:\.\.\/)+submodule\/bun\//, replacement: `${process.env.FNPM_BUN_DIR}/` }]
              : []),
          ],
        },
        test: {
          name: "conformance",
          globals: true,
          root: here("."),
          include: ["conformance/install/**/*.test.ts"],
          setupFiles: [here("conformance/compat/setup.ts")],
          globalSetup: [here("conformance/global-setup.ts")],
          testTimeout: 60_000,
          hookTimeout: 60_000,
          pool: "forks",
        },
      },
    ],
  },
});
