import { SilentError } from "../errors.ts";
import { longName, shortName, takesValue } from "./registry.ts";
import type { FlagSpec } from "./registry-types.ts";

/**
 * Parse a command's own tokens against its FlagSpecs.
 *
 * This, not cac's parsed options, is where every command reads its flags:
 * values stay the exact strings typed (mri would turn `--otp 007` into 7), and
 * the arity rules follow bun's clap:
 *   - `--flag value` and `--flag=value` for value flags,
 *   - bun's `<STR>?` (`optionalValue`): the value only attaches with `=`,
 *   - `-abc` clusters of short switches, and `-Fvalue` / `-F value` for a
 *     short value flag,
 *   - `--no-<flag>` negates a boolean,
 *   - repeatable flags collect into arrays; otherwise the last one wins.
 *
 * Unknown flags never reach here: `rejectUnknownFlags` has already run.
 */

export type ParsedFlags = {
  /** Keyed by FlagSpec.key. Booleans are booleans, value flags strings or string[]. */
  values: Record<string, boolean | string | string[]>;
  positionals: string[];
};

type Index = { long: Map<string, FlagSpec>; short: Map<string, FlagSpec> };

const indexCache = new WeakMap<readonly FlagSpec[], Index>();

function indexSpecs(specs: readonly FlagSpec[]): Index {
  const cached = indexCache.get(specs);
  if (cached) {
    return cached;
  }
  const long = new Map<string, FlagSpec>();
  const short = new Map<string, FlagSpec>();
  for (const spec of specs) {
    const name = longName(spec.raw);
    if (!long.has(name)) {
      long.set(name, spec);
    }
    const letter = shortName(spec.raw);
    if (letter !== undefined && !short.has(letter)) {
      short.set(letter, spec);
    }
  }
  const built = { long, short };
  indexCache.set(specs, built);
  return built;
}

const BOOLEAN_WORDS: Record<string, boolean> = {
  true: true,
  false: false,
  "1": true,
  "0": false,
  yes: true,
  no: false,
};

export function parseFlags(tokens: readonly string[], specs: readonly FlagSpec[]): ParsedFlags {
  const index = indexSpecs(specs);
  const values: ParsedFlags["values"] = {};
  const positionals: string[] = [];

  const assign = (spec: FlagSpec, value: boolean | string): void => {
    if (spec.repeatable && typeof value === "string") {
      const existing = values[spec.key];
      values[spec.key] = Array.isArray(existing) ? [...existing, value] : [value];
      return;
    }
    values[spec.key] = value;
  };

  for (let position = 0; position < tokens.length; position += 1) {
    const token = tokens[position] as string;

    if (token.startsWith("--") && token.length > 2) {
      const equals = token.indexOf("=");
      const name = equals === -1 ? token.slice(2) : token.slice(2, equals);
      const inline = equals === -1 ? undefined : token.slice(equals + 1);
      let spec = index.long.get(name);
      let negated = false;
      if (spec === undefined && name.startsWith("no-")) {
        const positive = index.long.get(name.slice(3));
        if (positive !== undefined && !takesValue(positive)) {
          spec = positive;
          negated = true;
        }
      }
      if (spec === undefined) {
        throw new SilentError(`Unknown flag \`--${name}\`.`, { code: "UNKNOWN_FLAG" });
      }

      if (!takesValue(spec)) {
        if (inline === undefined) {
          assign(spec, !negated);
          continue;
        }
        const parsed = BOOLEAN_WORDS[inline.toLowerCase()];
        if (parsed === undefined || negated) {
          throw new SilentError(`\`--${name}\` does not take a value (got "${inline}").`, {
            code: "INVALID_FLAG_VALUE",
          });
        }
        assign(spec, parsed);
        continue;
      }

      if (inline !== undefined) {
        assign(spec, inline);
        continue;
      }
      if (spec.optionalValue) {
        assign(spec, true);
        continue;
      }
      const next = tokens[position + 1];
      if (next === undefined || (next.startsWith("-") && next !== "-")) {
        throw new SilentError(`\`--${name}\` requires a value.`, { code: "MISSING_ARGUMENT" });
      }
      assign(spec, next);
      position += 1;
      continue;
    }

    if (token.startsWith("-") && token.length > 1 && token !== "--") {
      const body = token.slice(1);
      for (let offset = 0; offset < body.length; offset += 1) {
        const letter = body[offset] as string;
        const spec = index.short.get(letter);
        if (spec === undefined) {
          throw new SilentError(`Unknown flag \`-${letter}\`.`, { code: "UNKNOWN_FLAG" });
        }
        if (!takesValue(spec)) {
          assign(spec, true);
          continue;
        }
        // A short value flag swallows the rest of its cluster: `-Fapp`, `-F=app`.
        const rest = body.slice(offset + 1).replace(/^=/, "");
        if (rest !== "") {
          assign(spec, rest);
        } else if (spec.optionalValue) {
          assign(spec, true);
        } else {
          const next = tokens[position + 1];
          if (next === undefined || (next.startsWith("-") && next !== "-")) {
            throw new SilentError(`\`-${letter}\` requires a value.`, {
              code: "MISSING_ARGUMENT",
            });
          }
          assign(spec, next);
          position += 1;
        }
        break;
      }
      continue;
    }

    positionals.push(token);
  }

  return { values, positionals };
}

/** Typed readers over ParsedFlags.values, so commands never cast. */
export const read = {
  bool(values: ParsedFlags["values"], key: string): boolean {
    return values[key] === true;
  },
  /** `undefined` when absent; `true` for an optional-value flag passed bare. */
  maybeBool(values: ParsedFlags["values"], key: string): boolean | undefined {
    const value = values[key];
    return typeof value === "boolean" ? value : undefined;
  },
  string(values: ParsedFlags["values"], key: string): string | undefined {
    const value = values[key];
    if (Array.isArray(value)) {
      return value.at(-1);
    }
    return typeof value === "string" ? value : undefined;
  },
  strings(values: ParsedFlags["values"], key: string): string[] {
    const value = values[key];
    if (Array.isArray(value)) {
      return value;
    }
    return typeof value === "string" ? [value] : [];
  },
};
