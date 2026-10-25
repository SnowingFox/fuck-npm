/**
 * vitest setup for the conformance suite: the `Bun` global, and the matchers that bun:test has and vitest does not
 * (`toBeTrue`, `toEndWith`, ... plus the harness matchers `toHaveBins`, `toBeValidBin`, `toBeWorkspaceLink`, `toMatchNodeModulesAt`).
 */
import { syncBuiltinESMExports } from "node:module";
import path from "node:path";
import { expect } from "vitest";
import { toBeValidBin, toBeWorkspaceLink, toHaveBins, toMatchNodeModulesAt } from "../harness.ts";
import { Bun } from "./bun.ts";
import * as bunTest from "./bun-test.ts";

(globalThis as { Bun?: unknown }).Bun = Bun;

// `tempDir()` returns a String object, which bun's `path` functions take like a string and node's reject. Unwrap it.
{
  const unwrap = (value: unknown) => (value instanceof String ? value.valueOf() : value);
  for (const p of [path, path.posix]) {
    for (const name of ["join", "resolve", "relative", "dirname", "basename", "extname", "normalize", "isAbsolute"] as const) {
      const original = p[name] as (...args: unknown[]) => unknown;
      Reflect.set(p, name, (...args: unknown[]) => original(...args.map(unwrap)));
    }
  }
  syncBuiltinESMExports();
}
// bun:test names are globals in bun; vitest `globals: true` installs its own, replace them with the bun flavour.
Object.assign(globalThis, {
  test: bunTest.test,
  it: bunTest.it,
  describe: bunTest.describe,
  setDefaultTimeout: bunTest.setDefaultTimeout,
  jest: bunTest.jest,
  mock: bunTest.mock,
  spyOn: bunTest.spyOn,
});

type Result = { pass: boolean; message: () => string };

function result(pass: boolean, message: string): Result {
  return { pass, message: () => message };
}

const show = (value: unknown) => {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
};

expect.extend({
  toBeTrue(received: unknown) {
    return result(received === true, `expected ${show(received)} to be true`);
  },
  toBeFalse(received: unknown) {
    return result(received === false, `expected ${show(received)} to be false`);
  },
  toBeString(received: unknown) {
    return result(typeof received === "string", `expected ${show(received)} to be a string`);
  },
  toBeNumber(received: unknown) {
    return result(typeof received === "number", `expected ${show(received)} to be a number`);
  },
  toBeArray(received: unknown) {
    return result(Array.isArray(received), `expected ${show(received)} to be an array`);
  },
  toBeArrayOfSize(received: unknown, size: number) {
    return result(Array.isArray(received) && received.length === size, `expected ${show(received)} to have ${size} items`);
  },
  toBeEmpty(received: unknown) {
    let empty = false;
    if (typeof received === "string" || Array.isArray(received)) empty = received.length === 0;
    else if (received instanceof Map || received instanceof Set) empty = received.size === 0;
    else if (received !== null && typeof received === "object") empty = Object.keys(received).length === 0;
    return result(empty, `expected ${show(received)} to be empty`);
  },
  toBeNil(received: unknown) {
    return result(received === null || received === undefined, `expected ${show(received)} to be null or undefined`);
  },
  toStartWith(received: unknown, prefix: string) {
    return result(typeof received === "string" && received.startsWith(prefix), `expected ${show(received)} to start with ${show(prefix)}`);
  },
  toEndWith(received: unknown, suffix: string) {
    return result(typeof received === "string" && received.endsWith(suffix), `expected ${show(received)} to end with ${show(suffix)}`);
  },
  toInclude(received: unknown, part: string) {
    return result(typeof received === "string" && received.includes(part), `expected ${show(received)} to include ${show(part)}`);
  },
  toContainKey(received: unknown, key: string) {
    return result(received !== null && typeof received === "object" && key in received, `expected ${show(received)} to contain key ${show(key)}`);
  },
  toContainKeys(received: unknown, keys: string[]) {
    return result(received !== null && typeof received === "object" && keys.every(key => key in (received as object)), `expected ${show(received)} to contain keys ${show(keys)}`);
  },
  toContainAnyKeys(received: unknown, keys: string[]) {
    return result(received !== null && typeof received === "object" && keys.some(key => key in (received as object)), `expected ${show(received)} to contain any of ${show(keys)}`);
  },
  toContainAnyValues(received: unknown, values: unknown[]) {
    const own = received !== null && typeof received === "object" ? Object.values(received) : [];
    return result(values.some(value => own.includes(value)), `expected ${show(received)} to contain any of ${show(values)}`);
  },
  toBeWithin(received: unknown, start: number, end: number) {
    return result(typeof received === "number" && received >= start && received < end, `expected ${show(received)} to be within [${start}, ${end})`);
  },
  toBeOneOf(received: unknown, values: unknown[]) {
    return result(values.includes(received), `expected ${show(received)} to be one of ${show(values)}`);
  },
  toSatisfy(received: unknown, predicate: (value: unknown) => boolean) {
    return result(predicate(received), `expected ${show(received)} to satisfy the predicate`);
  },
  toHaveBins,
  toBeValidBin,
  toBeWorkspaceLink,
  toMatchNodeModulesAt,
});

// bun: `expect.unreachable(message?)`
(expect as unknown as { unreachable: (message?: string) => never }).unreachable = (message?: string) => {
  throw new Error(message ?? "reached unreachable code");
};
