import realSemver from "semver";
type Call = { f: string; a: string; b: string; node?: boolean };
let pending: Call[] = [];
let current = "";
const records = new Map<string, any>();
const unhandled: string[] = [];
const real = Bun.semver;
function wrap(f: string, impl: (a: any, b: any) => any) {
  const fn = function (this: any, ...args: any[]) {
    let a: string, b: string;
    try {
      a = String(args[0]);
      b = String(args[1]);
    } catch {
      return impl.apply(this, args as any);
    }
    const call: Call = { f, a, b };
    pending.push(call);
    return impl.apply(this, args as any);
  };
  Object.defineProperty(fn, "name", { value: f });
  return fn;
}
export const semverShim = {
  satisfies: wrap("satisfies", (a, b) => real.satisfies(a, b)),
  order: wrap("order", (a, b) => real.order(a, b)),
};
export const semverIntersects = wrap("intersects", () => false);
class R extends realSemver.Range {
  test(v: any) {
    const r = super.test(v);
    const last = pending[pending.length - 1];
    if (last && last.f === "satisfies") last.node = r;
    return r;
  }
}
export const nodeSemver = { ...realSemver, Range: R };
function record(c: Call, e: any) {
  const key = JSON.stringify([c.f, c.a, c.b]);
  const prev = records.get(key);
  if (prev !== undefined && prev.e !== e) unhandled.push(`CONFLICT ${key} ${prev.e} ${e} in ${current}`);
  records.set(key, { f: c.f, a: c.a, b: c.b, e, t: current });
}
function handle(actual: any, e: any, calls: Call[]) {
  if (calls.length === 0) return;
  if (calls.length === 1 && (typeof e === "boolean" || typeof e === "number")) return record(calls[0], e);
  if (Array.isArray(e) && e.length === 0 && calls.every(c => c.f === "satisfies")) {
    for (const c of calls) record(c, false);
    return;
  }
  if (Array.isArray(e) && e.length === calls.length) {
    e.forEach((ex: any, i: number) => {
      const val = Array.isArray(ex) ? ex[ex.length - 1] : ex && typeof ex === "object" ? ex.satisfies : ex;
      if (typeof val === "boolean") record(calls[i], val);
      else unhandled.push(`table value ${JSON.stringify(ex)} in ${current}`);
    });
    return;
  }
  if (e && typeof e === "object" && Array.isArray(e.disagreements) && e.disagreements.length === 0) {
    for (const c of calls) if (c.f === "satisfies") record(c, c.node ?? false);
    return;
  }
  unhandled.push(`unhandled expect with ${calls.length} calls in ${current}`);
}
export function expect(actual?: any) {
  const calls = pending.splice(0);
  const m: any = {
    toBe: (e: any) => handle(actual, e, calls),
    toEqual: (e: any) => handle(actual, e, calls),
    toBeTrue: () => handle(actual, true, calls),
    toBeFalse: () => handle(actual, false, calls),
    toThrow: () => {
      handle(actual, undefined, calls);
      try {
        actual();
      } catch {}
      for (const c of pending.splice(0)) record(c, "throw");
    },
  };
  return new Proxy(m, { get: (t, p) => (p in t ? t[p] : () => {}) });
}
(expect as any).objectContaining = (x: any) => x;
const skipped: string[] = [];
function run(name: string, fn: Function, args: any[] = []) {
  const src = fn.toString();
  if (/Bun\.spawn|Bun\.serve|manifestRegistry|Bun\.gc|async/.test(src)) {
    skipped.push(name);
    return;
  }
  current = name;
  pending = [];
  try {
    fn(...args);
  } catch (err) {
    unhandled.push(`threw in ${name}: ${err}`);
  }
  if (pending.length) unhandled.push(`${pending.length} dangling calls in ${name}`);
  pending = [];
}
function each(rows: any[]) {
  return (name: string, fn: Function) => {
    for (const row of rows) run(`${name} ${JSON.stringify(row)}`, fn, Array.isArray(row) ? row : [row]);
  };
}
export const test: any = (name: string, fn: Function) => run(name, fn);
test.each = each;
test.concurrent = test;
export const it = test;
export const describe: any = (name: string, fn: Function) => fn();
describe.concurrent = describe;
describe.each = (rows: any[]) => (name: string, fn: Function) => rows.forEach(r => fn(...(Array.isArray(r) ? r : [r])));
export const bunEnv = {};
export const bunExe = () => "bun";
export const isASAN = false;
export const isDebug = false;
export const tempDir = () => "";
export function finish(out: string) {
  const list = [...records.values()];
  require("fs").writeFileSync(out, JSON.stringify(list, null, 0).replace(/\},\{/g, "},\n{") + "\n");
  console.error(`records=${list.length} skipped=${skipped.length}`);
  for (const s of skipped) console.error("SKIP", s);
  for (const u of unhandled) console.error("UNHANDLED", u);
}
