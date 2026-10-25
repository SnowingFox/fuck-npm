/**
 * `bun:test` on top of vitest: the same names, plus the bun-only modifiers the install tests use
 * (`test.if`, `test.todoIf`, `test.failing`, `test.serial`, `setDefaultTimeout`, `jest`, `mock`).
 *
 * `test.concurrent` / `describe.concurrent` run sequentially unless FNPM_CONFORMANCE_CONCURRENT=1: vitest
 * attributes snapshots of concurrent tests through the test context, and the ported bodies use the global
 * `expect`. Files still run in parallel across workers.
 */
import * as vitest from "vitest";

export {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  expect,
  expectTypeOf,
  onTestFailed,
  onTestFinished,
  vi,
} from "vitest";

const concurrent = process.env.FNPM_CONFORMANCE_CONCURRENT === "1";

type Fn = (...args: any[]) => any;
type Options = number | { timeout?: number; retry?: number; repeats?: number };

/** bun takes `(name, fn, options)`; vitest 4 takes an options object only as the second argument. */
function call(base: any, name: string, fn: Fn | undefined, options: Options | undefined) {
  if (options !== null && typeof options === "object") return base(name, options, fn);
  return base(name, fn, options);
}

function each(base: any) {
  return (table: unknown) => {
    const run = base.each(table);
    return (name: string, fn: Fn, options?: Options) => call(run, name, fn, options);
  };
}

interface BunTest {
  (name: string, fn?: Fn, options?: Options): void;
  only: BunTest;
  skip: BunTest;
  todo: BunTest;
  failing: BunTest;
  concurrent: BunTest;
  serial: BunTest;
  each: Fn;
  if(condition: unknown): BunTest;
  skipIf(condition: unknown): BunTest;
  todoIf(condition: unknown): BunTest;
  failingIf(condition: unknown): BunTest;
  runIf(condition: unknown): BunTest;
}

function makeTest(base: any): BunTest {
  const t = ((name: string, fn?: Fn, options?: Options) => {
    if (fn === undefined) return base.todo(name);
    return call(base, name, fn, options);
  }) as BunTest;
  const todo = ((name: string) => base.todo(name)) as BunTest;
  Object.assign(todo, { each: (table: unknown) => (name: string) => base.todo(name) });
  Object.defineProperties(t, {
    only: { get: () => makeTest(base.only) },
    skip: { get: () => makeTest(base.skip) },
    todo: { get: () => makeTestLike(todo, base) },
    failing: { get: () => makeTest(base.fails) },
    concurrent: { get: () => (concurrent ? makeTest(base.concurrent) : t) },
    serial: { get: () => (base.sequential ? makeTest(base.sequential) : t) },
  });
  t.each = each(base);
  t.if = condition => (condition ? t : makeTest(base.skip));
  t.runIf = t.if;
  t.skipIf = condition => (condition ? makeTest(base.skip) : t);
  t.todoIf = condition => (condition ? makeTestLike(todo, base) : t);
  t.failingIf = condition => (condition ? makeTest(base.fails) : t);
  return t;
}

/** A todo collector that still answers the modifier chain (`test.todo.each(...)`, ...). */
function makeTestLike(todo: BunTest, base: any): BunTest {
  const self = todo;
  for (const key of ["if", "skipIf", "todoIf", "failingIf", "runIf"] as const) {
    (self as any)[key] = () => self;
  }
  for (const key of ["only", "skip", "todo", "failing", "concurrent", "serial"] as const) {
    Object.defineProperty(self, key, { get: () => self, configurable: true });
  }
  self.each = (table: unknown) => (name: string) => base.todo(name);
  return self;
}

interface BunDescribe {
  (name: string, fn: Fn): void;
  only: BunDescribe;
  skip: BunDescribe;
  todo: BunDescribe;
  concurrent: BunDescribe;
  serial: BunDescribe;
  each: Fn;
  if(condition: unknown): BunDescribe;
  skipIf(condition: unknown): BunDescribe;
  todoIf(condition: unknown): BunDescribe;
}

function makeDescribe(base: any): BunDescribe {
  const d = ((name: string, fn: Fn) => base(name, fn)) as BunDescribe;
  Object.defineProperties(d, {
    only: { get: () => makeDescribe(base.only) },
    skip: { get: () => makeDescribe(base.skip) },
    todo: { get: () => makeDescribe(base.skip) },
    concurrent: { get: () => (concurrent ? makeDescribe(base.concurrent) : d) },
    serial: { get: () => (base.sequential ? makeDescribe(base.sequential) : d) },
  });
  d.each = each(base);
  d.if = condition => (condition ? d : makeDescribe(base.skip));
  d.skipIf = condition => (condition ? makeDescribe(base.skip) : d);
  d.todoIf = condition => (condition ? makeDescribe(base.skip) : d);
  return d;
}

export const test = makeTest(vitest.test);
export const it = makeTest(vitest.it);
export const describe = makeDescribe(vitest.describe);

/** bun applies it to the rest of the file; vitest config is per worker, which is the same for one file. */
export function setDefaultTimeout(ms: number) {
  vitest.vi.setConfig({ testTimeout: ms, hookTimeout: ms });
}

export const mock = Object.assign((fn?: Fn) => vitest.vi.fn(fn), {
  module: (path: string, factory: () => unknown) => vitest.vi.doMock(path, factory as never),
  restore: () => vitest.vi.restoreAllMocks(),
});

export const spyOn = vitest.vi.spyOn;

export const jest = {
  fn: (fn?: Fn) => vitest.vi.fn(fn),
  spyOn: vitest.vi.spyOn,
  restoreAllMocks: () => vitest.vi.restoreAllMocks(),
  clearAllMocks: () => vitest.vi.clearAllMocks(),
  resetAllMocks: () => vitest.vi.resetAllMocks(),
  useFakeTimers: () => vitest.vi.useFakeTimers(),
  useRealTimers: () => vitest.vi.useRealTimers(),
  setTimeout: (ms: number) => setDefaultTimeout(ms),
};
