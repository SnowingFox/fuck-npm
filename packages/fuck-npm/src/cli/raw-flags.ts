/**
 * Recover flag values that mri destroyed.
 *
 * cac parses with mri, which coerces anything numeric-looking into a JS
 * number: `--otp 012345` arrives as 12345, `--tag 1e3` as 1000, a long
 * registry token loses digits. So no command reads a string flag from cac's
 * parsed options. Values come from argv, the only place the user's characters
 * survive: `parse-flags.ts` reads tokens directly, and these helpers serve
 * code that only has the ambient argv.
 */

export type RawFlagLookup = (key: string) => string | undefined;

/**
 * Index argv by flag name. Handles `--flag value`, `--flag=value` and short
 * aliases, and stops at `--`, since everything after it belongs to a child.
 */
export function indexRawFlags(argv: string[]): Map<string, string[]> {
  const raw = new Map<string, string[]>();
  const args = argv.slice(2);
  const stop = args.indexOf("--");
  const scannable = stop === -1 ? args : args.slice(0, stop);

  for (let index = 0; index < scannable.length; index += 1) {
    const token = scannable[index] as string;
    if (!token.startsWith("-") || token === "-") {
      continue;
    }
    const equals = token.indexOf("=");
    if (equals !== -1) {
      push(raw, normalizeFlagName(token.slice(0, equals)), token.slice(equals + 1));
      continue;
    }
    const next = scannable[index + 1];
    // A following token that is itself a flag means this one is boolean.
    if (next === undefined || (next.startsWith("-") && next !== "-")) {
      continue;
    }
    push(raw, normalizeFlagName(token), next);
    index += 1;
  }
  return raw;
}

function push(into: Map<string, string[]>, key: string, value: string): void {
  const existing = into.get(key);
  if (existing) {
    existing.push(value);
    return;
  }
  into.set(key, [value]);
}

/** camelCase a flag token the way cac keys its options (`--cache-dir` → `cacheDir`). */
export function normalizeFlagName(token: string): string {
  const name = token.replace(/^-+/, "");
  return name.replaceAll(/-([a-z0-9])/g, (_, char: string) => char.toUpperCase());
}

export function rawFlagLookup(argv: string[]): RawFlagLookup {
  const index = indexRawFlags(argv);
  return (key: string) => index.get(key)?.[0];
}

/**
 * Flag names present in argv at all, with or without a value. Presence is its
 * own question for negatable booleans: cac defaults a flag with a `--no-`
 * sibling to true, so absent and explicitly passed arrive identically.
 */
export function indexRawFlagNames(argv: string[]): Set<string> {
  const names = new Set<string>();
  const args = argv.slice(2);
  const stop = args.indexOf("--");
  const scannable = stop === -1 ? args : args.slice(0, stop);
  for (const token of scannable) {
    if (!token.startsWith("-") || token === "-") {
      continue;
    }
    const equals = token.indexOf("=");
    names.add(normalizeFlagName(equals === -1 ? token : token.slice(0, equals)));
  }
  return names;
}

/**
 * argv for the invocation being dispatched. Module-level so a helper deep in
 * a command need not have it threaded through every signature; `runParsedCli`
 * sets it on entry, so several invocations in one process each see their own.
 */
let currentArgv: string[] = [];

export function setCurrentArgv(argv: string[]): void {
  currentArgv = argv;
}

export function rawFlagValue(key: string): string | undefined {
  return indexRawFlags(currentArgv).get(key)?.[0];
}

export function rawFlagValues(key: string): string[] {
  return indexRawFlags(currentArgv).get(key) ?? [];
}

export function rawFlagPresent(key: string): boolean {
  return indexRawFlagNames(currentArgv).has(key);
}
