import { SilentError } from "../errors.ts";
import { longName, shortName, visibleChildren } from "../cli/registry.ts";
import type { CommandNode, Registry } from "../cli/registry-types.ts";
import type { CommandContext } from "./context.ts";

/**
 * `fnpm completions [bash|zsh|fish]`: a static script generated from the
 * registry, so a new command or flag is completed without touching this file.
 */

export type Shell = "bash" | "zsh" | "fish";
const SHELLS: Shell[] = ["bash", "zsh", "fish"];

type Entry = { path: string[]; subcommands: string[]; flags: string[] };

/** Every visible command path with its subcommand names and flag spellings. */
export function completionEntries(registry: Registry): Entry[] {
  const entries: Entry[] = [];
  const walk = (node: CommandNode, path: string[]): void => {
    if (node.kind === "group") {
      const children = visibleChildren(node);
      const subcommands = children.flatMap((child) => [child.name, ...(child.aliases ?? [])]);
      if (path.length === 0) {
        subcommands.push(...registry.aliases.map((alias) => alias.name));
      }
      entries.push({ path, subcommands, flags: flagWords(registry, []) });
      for (const child of children) {
        walk(child, [...path, child.name]);
        for (const alias of child.aliases ?? []) {
          walk(child, [...path, alias]);
        }
      }
      return;
    }
    entries.push({
      path,
      subcommands: [],
      flags: flagWords(registry, node.flags.filter((spec) => spec.hidden !== true)),
    });
  };
  walk(registry.root, []);
  for (const alias of registry.aliases) {
    const target = entries.find((entry) => entry.path.join(" ") === alias.target.join(" "));
    if (target) entries.push({ ...target, path: [alias.name] });
  }
  return entries;
}

function flagWords(registry: Registry, specs: readonly { raw: string; hidden?: boolean }[]): string[] {
  const words: string[] = [];
  for (const spec of [...specs, ...registry.globalFlags.filter((flag) => flag.hidden !== true)]) {
    words.push(`--${longName(spec.raw)}`);
    const short = shortName(spec.raw);
    if (short !== undefined) words.push(`-${short}`);
  }
  return [...new Set(words)];
}

const fnName = (bin: string): string => `_${bin.replaceAll(/[^a-zA-Z0-9]/g, "_")}`;

export function bashScript(registry: Registry): string {
  const entries = completionEntries(registry).sort((a, b) => b.path.length - a.path.length);
  const cases = entries
    .map((entry) => {
      const key = entry.path.join(" ");
      const pattern = key === "" ? '""' : `"${key}"|"${key} "*`;
      return `    ${pattern}) cmds="${entry.subcommands.join(" ")}"; flags="${entry.flags.join(" ")}" ;;`;
    })
    .join("\n");
  const name = fnName(registry.bin);
  return `# ${registry.bin} completions for bash. Add to ~/.bashrc:
#   eval "$(${registry.bin} completions bash)"
${name}() {
  local cur="\${COMP_WORDS[COMP_CWORD]}" path="" word cmds="" flags=""
  local i
  for ((i = 1; i < COMP_CWORD; i++)); do
    word="\${COMP_WORDS[i]}"
    case "$word" in -*) ;; *) path="\${path:+$path }$word" ;; esac
  done
  case "$path" in
${cases}
  esac
  if [[ "$cur" == -* ]]; then
    COMPREPLY=($(compgen -W "$flags" -- "$cur"))
  else
    COMPREPLY=($(compgen -W "$cmds" -- "$cur"))
  fi
}
complete -o default -F ${name} ${registry.bin}
`;
}

export function zshScript(registry: Registry): string {
  return `# ${registry.bin} completions for zsh. Add to ~/.zshrc:
#   eval "$(${registry.bin} completions zsh)"
autoload -U +X bashcompinit && bashcompinit
${bashScript(registry).split("\n").filter((line) => !line.startsWith("#")).join("\n")}`;
}

export function fishScript(registry: Registry): string {
  const bin = registry.bin;
  const lines = [
    `# ${bin} completions for fish:`,
    `#   ${bin} completions fish > ~/.config/fish/completions/${bin}.fish`,
    `complete -c ${bin} -f`,
  ];
  for (const entry of completionEntries(registry)) {
    const condition =
      entry.path.length === 0
        ? "__fish_use_subcommand"
        : `__fish_seen_subcommand_from ${entry.path.at(-1)}`;
    if (entry.subcommands.length > 0) {
      lines.push(`complete -c ${bin} -n '${condition}' -a '${entry.subcommands.join(" ")}'`);
    }
    if (entry.path.length > 0) {
      const longs = entry.flags.filter((flag) => flag.startsWith("--")).map((flag) => flag.slice(2));
      if (longs.length > 0) {
        lines.push(`complete -c ${bin} -n '${condition}' ${longs.map((flag) => `-l ${flag}`).join(" ")}`);
      }
    }
  }
  return `${lines.join("\n")}\n`;
}

export function detectShell(env: NodeJS.ProcessEnv): Shell | undefined {
  const shell = env.SHELL?.split("/").at(-1);
  return SHELLS.find((candidate) => candidate === shell);
}

export async function runCompletions(ctx: CommandContext): Promise<number> {
  const requested = ctx.positionals[0] ?? detectShell(ctx.runtime.env);
  const shell = SHELLS.find((candidate) => candidate === requested);
  if (shell === undefined) {
    throw new SilentError(
      requested === undefined
        ? "Could not detect your shell. Pass one: fnpm completions bash|zsh|fish"
        : `Unsupported shell "${requested}". Supported: ${SHELLS.join(", ")}.`,
      { code: "INVALID_FLAG_VALUE" },
    );
  }
  const script =
    shell === "bash" ? bashScript(ctx.registry) : shell === "zsh" ? zshScript(ctx.registry) : fishScript(ctx.registry);
  if (ctx.output.mode === "json") {
    ctx.output.json({ ok: true, shell, script });
  } else {
    ctx.runtime.stdout.write(script);
  }
  return 0;
}
