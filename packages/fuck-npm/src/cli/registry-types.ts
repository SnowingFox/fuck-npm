/**
 * One flag as the CLI declares it.
 *
 * `raw` is the cac spelling (`"-c, --config [path]"`). It is what help prints
 * and what gets registered on cac. Value flags always use `[brackets]`: with
 * `<angles>`, cac's negation default trips its own required-value check on
 * every invocation of the command.
 */
export type FlagSpec = {
  /** camelCase of the long name (`networkConcurrency`, `noSave`). */
  key: string;
  raw: string;
  description: string;
  /**
   * bun's `<STR>?`: the value only attaches with `=`. `--catalog` alone is a
   * switch, `--catalog=react19` names one.
   */
  optionalValue?: boolean;
  /** Repeatable; occurrences collect into an array (`--ca a --ca b`). */
  repeatable?: boolean;
  /** Accepted on the command line but kept out of help. */
  hidden?: boolean;
};

/** A titled group of example invocations, each paired with why you would run it. */
export type ExampleGroup = {
  title: string;
  examples: [command: string, why: string][];
};

/** One positional argument in a command's usage line. */
export type PositionalSpec = {
  name: string;
  required: boolean;
  /** Consumes the remaining positionals (`[...packages]`). */
  variadic?: boolean;
  description: string;
};

type CommandCommon = {
  /** Path segment only: `"rm"`, never `"pm cache rm"`. */
  name: string;
  aliases?: string[];
  description: string;
  /** Overrides the usage line derived from the path and positionals. */
  usageText?: string;
  exampleGroups?: ExampleGroup[];
  notes?: string[];
  /** Resolvable and runnable, but omitted from help, `--tree` and completions. */
  hidden?: boolean;
};

/** A runnable command. */
export type CommandLeaf = CommandCommon & {
  kind: "leaf";
  positionals?: PositionalSpec[];
  flags: readonly FlagSpec[];
  /**
   * Forward arguments to a child process instead of parsing them.
   *
   * A number is bun's `stop_after_positional_at`: our flags are parsed until
   * that many positionals have been seen, and everything after goes to the
   * child untouched (`fnpm run build --watch` hands `--watch` to the script).
   * `true` forwards every token that is not one of our declared flags.
   */
  passthrough?: true | number;
  /**
   * Hand every token after the command (and the global flags typed before it) to the core
   * unparsed: `fnpm x` and `fnpm create` parse their own arguments like bunx does.
   */
  rawArgs?: true;
  /**
   * npm/yarn/pnpm flag translation (bun's FOREIGN_FLAGS). `"update"` applies
   * the table minus the aliases that would select update groups.
   */
  foreignFlags?: "install" | "update";
  /** Silently drop bun's runtime-only switches (`--bun`, `--watch`, ...). */
  ignoresRuntimeFlags?: boolean;
};

/** A namespace whose children are the runnable commands. */
export type CommandGroup = CommandCommon & {
  kind: "group";
  children: CommandNode[];
  /** Child that runs when the group is invoked bare (`fnpm pm cache`). */
  defaultChild?: string;
};

export type CommandNode = CommandGroup | CommandLeaf;

/**
 * A top-level shorthand for a nested command (`fnpm whoami` →
 * `fnpm pm whoami`). Expanded before the tree walk, so the target renders its
 * own help and owns its own flags.
 */
export type CommandAlias = {
  kind: "alias";
  name: string;
  description: string;
  target: string[];
};

/** One bin's whole command surface. */
export type Registry = {
  /** The bin name, used in every usage line and hint. */
  bin: string;
  tagline: string;
  root: CommandGroup;
  aliases: readonly CommandAlias[];
  globalFlags: readonly FlagSpec[];
  /** Pointers to deeper help shown at the bottom of root help. */
  detailedHelp: [command: string, description: string][];
};

/** Result of walking a command path as far as the tree allows. */
export type ResolvedCommand = {
  /** Deepest node the path reached; the root group when nothing matched. */
  node: CommandNode;
  /** Canonical names of the segments the tree consumed, after alias expansion. */
  matched: string[];
  /** Segments left over: positionals for a leaf, a typo for a group. */
  rest: string[];
  viaAlias?: CommandAlias;
};
