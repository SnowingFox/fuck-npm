import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    name: "binding",
    include: ["tests/**/*.test.mjs"],
    testTimeout: 30_000,
    // One process: the addon is loaded once, like in the CLI.
    pool: "forks",
  },
});
