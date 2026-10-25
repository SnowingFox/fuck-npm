import path from "node:path";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "src"),
    },
  },
  test: {
    name: "unit",
    include: ["tests/unit/**/*.test.ts"],
    testTimeout: 30_000,
  },
});
