/**
 * Starts one fixture registry (bun's fixture packages + loose tarballs) for the whole conformance run. Workers
 * read its URL with `inject("fnpmRegistry")`; the harness writes it into a worker-local user npmrc.
 */
import type { TestProject } from "vitest/node";
import { start, type RunningRegistry } from "../registry/index.ts";

let registry: RunningRegistry | undefined;

export async function setup(project: TestProject) {
  registry = await start({ port: 0, recordRequests: false });
  process.env.FNPM_REGISTRY = registry.url;
  project.provide("fnpmRegistry", registry.url);
}

export async function teardown() {
  await registry?.stop();
}

declare module "vitest" {
  export interface ProvidedContext {
    fnpmRegistry: string;
  }
}
