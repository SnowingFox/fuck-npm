// Node entry for the security scanner. Packages arrive on fd 4; the result goes to fd 3.
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const scannerModuleName = __SCANNER_MODULE__;
const suppressError = __SUPPRESS_ERROR__;
const inputPath = process.platform === "win32" ? process.env.FNPM_SCANNER_INPUT : 4;
const outputPath = process.platform === "win32" ? process.env.FNPM_SCANNER_OUTPUT : 3;

// Scanners written for bun may call these.
if (typeof globalThis.Bun === "undefined") {
  globalThis.Bun = { sleep: ms => new Promise(resolve => setTimeout(resolve, ms)), semver: semver() };
}

// `Bun.semver`: node-semver's `satisfies` (ranges, `||`, hyphens, `~`, `^`, x-ranges; a
// prerelease only matches a comparator on the same version) and `order`.
function semver() {
  const parse = text => {
    const m = /^\s*v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?\s*$/.exec(String(text));
    return m && { v: [+m[1], +m[2], +m[3]], pre: m[4] ? m[4].split(".") : [] };
  };
  const comparePre = (a, b) => {
    if (!a.length || !b.length) return b.length - a.length;
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      if (a[i] === undefined) return -1;
      if (b[i] === undefined) return 1;
      const [x, y] = [a[i], b[i]];
      const [nx, ny] = [/^\d+$/.test(x), /^\d+$/.test(y)];
      if (x === y) continue;
      if (nx && ny) return +x - +y;
      if (nx) return -1;
      if (ny) return 1;
      return x < y ? -1 : 1;
    }
    return 0;
  };
  const compare = (a, b) => {
    for (let i = 0; i < 3; i++) if (a.v[i] !== b.v[i]) return a.v[i] - b.v[i];
    return comparePre(a.pre, b.pre);
  };
  // A partial version: numbers up to the first x/*, and its prerelease.
  const partial = text => {
    const m = /^v?(\d+|[xX*])?(?:\.(\d+|[xX*]))?(?:\.(\d+|[xX*]))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(text);
    if (!m) return null;
    const parts = [];
    for (const p of [m[1], m[2], m[3]]) {
      if (p === undefined || /^[xX*]$/.test(p)) break;
      parts.push(+p);
    }
    return { parts, pre: m[4] ? m[4].split(".") : [] };
  };
  const at = (parts, pre = []) => ({ v: [parts[0] ?? 0, parts[1] ?? 0, parts[2] ?? 0], pre });
  const bump = (parts, i) => at([...parts.slice(0, i), parts[i] + 1], ["0"]);
  const lower = (p, op) => [op, at(p.parts, p.pre)];
  // Comparators for one token: `[op, version]` pairs that all must hold.
  const desugar = token => {
    const m = /^(<=|>=|<|>|=|~>|~|\^)?\s*(.*)$/.exec(token);
    const op = m[1] ?? "";
    const p = partial(m[2]);
    if (!p) return null;
    const n = p.parts.length;
    if (op === "^") {
      if (n === 0) return [[">=", at([0, 0, 0])]];
      const i = p.parts.findIndex(x => x !== 0);
      const keep = i === -1 ? n - 1 : Math.min(i, n - 1);
      return [lower(p, ">="), ["<", bump(p.parts, keep)]];
    }
    if (op === "~" || op === "~>") {
      if (n === 0) return [[">=", at([0, 0, 0])]];
      return [lower(p, ">="), ["<", bump(p.parts, n === 1 ? 0 : 1)]];
    }
    if (n === 3) return [[op === "" ? "=" : op, at(p.parts, p.pre)]];
    if (n === 0) return op === "<" || op === ">" ? [["<", at([0, 0, 0])]] : [];
    switch (op) {
      case ">": return [[">=", bump(p.parts, n - 1)]];
      case ">=": return [[">=", at(p.parts)]];
      case "<": return [["<", at(p.parts, ["0"])]];
      case "<=": return [["<", bump(p.parts, n - 1)]];
      default: return [[">=", at(p.parts)], ["<", bump(p.parts, n - 1)]];
    }
  };
  const holds = (v, [op, c]) => {
    const d = compare(v, c);
    return op === "<" ? d < 0 : op === "<=" ? d <= 0 : op === ">" ? d > 0 : op === ">=" ? d >= 0 : d === 0;
  };
  const satisfies = (version, range) => {
    const v = parse(version);
    if (!v) return false;
    for (const set of String(range).split("||")) {
      let text = set.trim().replace(/(<=|>=|<|>|=|~>|~|\^)\s+/g, "$1");
      const hyphen = /^(\S+)\s+-\s+(\S+)$/.exec(text);
      if (hyphen) text = `>=${hyphen[1]} <=${hyphen[2]}`;
      const comparators = [];
      let ok = true;
      for (const token of text.split(/\s+/).filter(Boolean)) {
        const c = desugar(token);
        if (!c) ok = false;
        else comparators.push(...c);
      }
      if (!ok || !comparators.every(c => holds(v, c))) continue;
      if (v.pre.length && !comparators.some(([, c]) => c.pre.length && c.v.every((x, i) => x === v.v[i]))) continue;
      return true;
    }
    return false;
  };
  const order = (a, b) => {
    const [x, y] = [parse(a), parse(b)];
    if (!x || !y) return 0;
    return Math.sign(compare(x, y));
  };
  return { satisfies, order };
}

function sendAndExit(message) {
  fs.writeFileSync(outputPath, JSON.stringify(message));
  process.exit(message.type === "error" ? 1 : 0);
}

let packages;
try {
  packages = JSON.parse(fs.readFileSync(inputPath, "utf8"));
  if (!Array.isArray(packages)) {
    throw new Error("Expected packages to be an array");
  }
} catch (error) {
  sendAndExit({
    type: "error",
    code: "SCAN_FAILED",
    message: `Failed to parse packages JSON: ${error instanceof Error ? error.message : String(error)}`,
  });
}

const isPath =
  scannerModuleName.startsWith(".") || scannerModuleName.startsWith("/") || path.isAbsolute(scannerModuleName);
const specifier = isPath ? pathToFileURL(path.resolve(process.cwd(), scannerModuleName)).href : scannerModuleName;

let scanner;
try {
  const mod = await import(specifier);
  scanner = mod.scanner ?? mod.default?.scanner;
} catch (error) {
  if (typeof error === "object" && error !== null && "code" in error && error.code === "ERR_MODULE_NOT_FOUND") {
    if (!suppressError) {
      console.error(`\x1b[31merror: \x1b[0mFailed to import security scanner: \x1b[1m'${scannerModuleName}'`);
    }
    sendAndExit({ type: "error", code: "MODULE_NOT_FOUND", module: scannerModuleName });
  } else {
    sendAndExit({
      type: "error",
      code: "SCAN_FAILED",
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

try {
  if (typeof scanner !== "object" || scanner === null || typeof scanner.version !== "string") {
    throw new Error("Security scanner must export a 'scanner' object with a version property");
  }
  if (scanner.version !== "1") {
    sendAndExit({
      type: "error",
      code: "INVALID_VERSION",
      message: `Security scanner must be version 1, got version ${scanner.version}`,
    });
  }
  if (typeof scanner.scan !== "function") {
    throw new Error(`scanner.scan is not a function, got ${typeof scanner.scan}`);
  }
  const result = await scanner.scan({ packages });
  if (!Array.isArray(result)) {
    throw new Error("Security scanner must return an array of advisories");
  }
  sendAndExit({ type: "result", advisories: result });
} catch (error) {
  if (!suppressError) {
    console.error(error);
  }
  sendAndExit({
    type: "error",
    code: "SCAN_FAILED",
    message: error instanceof Error ? error.message : "Unknown error occurred",
  });
}
