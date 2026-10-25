/**
 * The picker of `fnpm update -i`, ported from bun's `update_interactive_command.rs`.
 *
 * Rows are grouped by dependency type with Current / Target / Latest (and Workspace on wide
 * terminals) columns; the checkbox and name are colored by how far the chosen version moves
 * (red: major, or a breaking 0.x change; yellow: minor; green: patch). Keys: Space toggles,
 * `a` selects all, `n` none, `i` inverts, `l` toggles Latest (every selected row right after
 * `a`), j/k and the arrows move, ←/→ pick Target/Latest, PgUp/PgDn page, the mouse wheel
 * scrolls, Enter (or the end of the input) confirms, Ctrl+C / Ctrl+D cancel.
 *
 * The state machine (`PickerState` + `press`) is separate from drawing so it is unit-tested
 * with key strings.
 */

export type PickerRow = {
  name: string;
  current: string;
  /** The newest version in the declared range. */
  update: string;
  latest: string;
  /** `dependencies`, `devDependencies`, ... */
  dependencyType: string;
  workspace?: string;
};

export type PickerState = {
  rows: PickerRow[];
  selected: boolean[];
  useLatest: boolean[];
  cursor: number;
  viewportStart: number;
  viewportHeight: number;
  /** The last key was `a`: `l` then flips every selected row. */
  toggleAll: boolean;
};

type Version = { major: number; minor: number; patch: number; prerelease: string[] };

function parse(v: string): Version | undefined {
  const m = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(v.trim());
  return m ? { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]), prerelease: m[4]?.split(".") ?? [] } : undefined;
}

function compare(a: string, b: string): number {
  const x = parse(a);
  const y = parse(b);
  if (!x || !y) return 0;
  const core = x.major - y.major || x.minor - y.minor || x.patch - y.patch;
  if (core) return core;
  if (x.prerelease.length === 0 || y.prerelease.length === 0) {
    return x.prerelease.length === y.prerelease.length ? 0 : x.prerelease.length === 0 ? 1 : -1;
  }
  for (let i = 0; i < Math.max(x.prerelease.length, y.prerelease.length); i++) {
    const l = x.prerelease[i];
    const r = y.prerelease[i];
    if (l === r) continue;
    if (l === undefined || r === undefined) return l === undefined ? -1 : 1;
    const ln = /^\d+$/.test(l);
    const rn = /^\d+$/.test(r);
    if (ln && rn) return l.length - r.length || (l < r ? -1 : 1);
    if (ln !== rn) return ln ? -1 : 1;
    return l < r ? -1 : 1;
  }
  return 0;
}

/** A `latest` older than the installed version can't be picked: `fnpm update` doesn't downgrade. */
export function latestIsNewer(row: PickerRow): boolean {
  return compare(row.latest, row.current) > 0;
}

export function picksLatest(state: PickerState, i: number): boolean {
  return state.useLatest[i] === true && latestIsNewer(state.rows[i] as PickerRow);
}

export function targetOf(state: PickerState, i: number): string {
  const row = state.rows[i] as PickerRow;
  return picksLatest(state, i) ? row.latest : row.update;
}

export type Color = "red" | "yellow" | "green";

export function colorOf(current: string, target: string): Color {
  const c = parse(current);
  const t = parse(target);
  if (!c || !t) return "green";
  if (t.major !== c.major) return "red";
  if (t.minor !== c.minor) return c.major === 0 ? "red" : "yellow";
  if (t.patch !== c.patch && c.major === 0 && c.minor === 0) return "red";
  return "green";
}

const GROUP_ORDER = ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"];

export function sortRows(rows: PickerRow[]): PickerRow[] {
  const priority = (t: string) => {
    const i = GROUP_ORDER.indexOf(t);
    return i === -1 ? GROUP_ORDER.length : i;
  };
  return [...rows].sort(
    (a, b) => priority(a.dependencyType) - priority(b.dependencyType) || a.name.localeCompare(b.name),
  );
}

export function initialState(rows: PickerRow[], terminalRows: number, latest: boolean): PickerState {
  const sorted = sortRows(rows);
  return {
    rows: sorted,
    selected: sorted.map(() => false),
    useLatest: sorted.map(() => latest),
    cursor: 0,
    viewportStart: 0,
    viewportHeight: viewportHeight(terminalRows, sorted),
    toggleAll: false,
  };
}

/** Rows that fit: the prompt line, the group headers and the scroll hints take the rest. */
export function viewportHeight(terminalRows: number, rows: PickerRow[]): number {
  const groups = new Set(rows.map((r) => r.dependencyType)).size;
  return Math.max(1, Math.min(rows.length, terminalRows - 2 * groups - 4));
}

function updateViewport(s: PickerState): void {
  if (s.cursor < s.viewportStart) s.viewportStart = s.cursor;
  else if (s.cursor >= s.viewportStart + s.viewportHeight) s.viewportStart = s.cursor - s.viewportHeight + 1;
}

function ensureCursorInViewport(s: PickerState): void {
  if (s.cursor < s.viewportStart) s.cursor = s.viewportStart;
  else if (s.cursor >= s.viewportStart + s.viewportHeight) s.cursor = s.viewportStart + s.viewportHeight - 1;
}

/** Selecting a row with nothing newer in range means its latest. */
function select(s: PickerState, i: number): void {
  const row = s.rows[i] as PickerRow;
  if (row.current === row.update) s.useLatest[i] = true;
}

export type KeyResult = "continue" | "confirm" | "cancel" | "noop";

/**
 * Applies the keys at the start of `input`; returns what happened and how many bytes it used
 * (an incomplete escape sequence uses none).
 */
export function press(s: PickerState, input: string): { result: KeyResult; used: number } {
  if (input.length === 0) return { result: "noop", used: 0 };
  const last = s.rows.length - 1;
  const c = input[0] as string;
  switch (c) {
    case "\n":
    case "\r":
      return { result: "confirm", used: 1 };
    case "\x03":
    case "\x04":
      return { result: "cancel", used: 1 };
    case " ":
      s.selected[s.cursor] = !s.selected[s.cursor];
      select(s, s.cursor);
      s.toggleAll = false;
      return { result: "continue", used: 1 };
    case "a":
    case "A":
      s.selected.fill(true);
      s.rows.forEach((_, i) => select(s, i));
      s.toggleAll = true;
      return { result: "continue", used: 1 };
    case "n":
    case "N":
      s.selected.fill(false);
      s.toggleAll = false;
      return { result: "continue", used: 1 };
    case "i":
    case "I":
      s.selected = s.selected.map((v) => !v);
      s.toggleAll = false;
      return { result: "continue", used: 1 };
    case "l":
    case "L": {
      const next = !s.useLatest[s.cursor];
      if (s.toggleAll) {
        s.selected.forEach((sel, i) => {
          if (sel) s.useLatest[i] = next;
        });
      } else {
        s.useLatest[s.cursor] = next;
        s.selected[s.cursor] = true;
      }
      return { result: "continue", used: 1 };
    }
    case "j":
      s.cursor = (s.cursor + 1) % s.rows.length;
      updateViewport(s);
      s.toggleAll = false;
      return { result: "continue", used: 1 };
    case "k":
      s.cursor = s.cursor === 0 ? last : s.cursor - 1;
      updateViewport(s);
      s.toggleAll = false;
      return { result: "continue", used: 1 };
    case "\x1b": {
      if (input.length === 1) return { result: "noop", used: 0 };
      if (input[1] !== "[") return { result: "continue", used: 1 };
      if (input.length < 3) return { result: "noop", used: 0 };
      const k = input[2] as string;
      s.toggleAll = false;
      switch (k) {
        case "A":
          s.cursor = s.cursor === 0 ? last : s.cursor - 1;
          updateViewport(s);
          return { result: "continue", used: 3 };
        case "B":
          s.cursor = (s.cursor + 1) % s.rows.length;
          updateViewport(s);
          return { result: "continue", used: 3 };
        case "C":
        case "D":
          s.useLatest[s.cursor] = k === "C";
          s.selected[s.cursor] = true;
          return { result: "continue", used: 3 };
        case "5":
        case "6": {
          if (input.length < 4) return { result: "noop", used: 0 };
          if (input[3] === "~") {
            s.cursor = k === "5" ? Math.max(0, s.cursor - s.viewportHeight) : Math.min(last, s.cursor + s.viewportHeight);
            updateViewport(s);
          }
          return { result: "continue", used: 4 };
        }
        case "<": {
          // SGR mouse: ESC [ < button ; col ; row (M|m)
          const end = input.slice(3).search(/[Mm]/);
          if (end === -1) return { result: "noop", used: 0 };
          const button = Number(input.slice(3, 3 + end).split(";")[0]);
          if (button === 64 && s.viewportStart > 0) {
            s.viewportStart -= 1;
            ensureCursorInViewport(s);
          } else if (button === 65 && s.viewportStart + s.viewportHeight < s.rows.length) {
            s.viewportStart += 1;
            ensureCursorInViewport(s);
          }
          return { result: "continue", used: 4 + end };
        }
        default:
          return { result: "continue", used: 3 };
      }
    }
    default:
      s.toggleAll = false;
      return { result: "continue", used: 1 };
  }
}

type Paint = {
  bold: (s: string) => string;
  dim: (s: string) => string;
  cyan: (s: string) => string;
  red: (s: string) => string;
  yellow: (s: string) => string;
  green: (s: string) => string;
  underline: (s: string) => string;
};

function truncate(text: string, width: number, onlyEnd = false): string {
  if (text.length <= width) return text;
  if (width <= 1) return "…".slice(0, width);
  if (onlyEnd) return `${text.slice(0, width - 1)}…`;
  return `${text.slice(0, width - 1)}…`;
}

const tagOf = (t: string) =>
  t === "devDependencies" ? " dev" : t === "peerDependencies" ? " peer" : t === "optionalDependencies" ? " optional" : "";

/** One frame of the picker, as lines (no trailing newline). */
export function render(s: PickerState, width: number, paint: Paint): string[] {
  const help =
    "Space to toggle, Enter to confirm, a to select all, n to select none, i to invert, l to toggle latest";
  const lines = [
    `${paint.cyan("?")} Select packages to update${paint.dim(` - ${truncate(help, Math.max(0, width - 30), true)}`)}`,
  ];
  const showWorkspace = width > 100 && s.rows.some((r) => r.workspace);
  const cap = (n: number) => Math.min(n, Math.max(10, Math.floor(width / 4)));
  const nameW = cap(Math.max(...s.rows.map((r) => r.name.length + tagOf(r.dependencyType).length), 7));
  const curW = Math.max(...s.rows.map((r) => r.current.length), 7);
  const tgtW = Math.max(...s.rows.map((r) => r.update.length), 6);
  const latW = Math.max(...s.rows.map((r) => r.latest.length), 6);
  const end = Math.min(s.viewportStart + s.viewportHeight, s.rows.length);
  if (s.rows.length > s.viewportHeight && s.viewportStart > 0) {
    lines.push(paint.dim(`  ↑ ${s.viewportStart} more package${s.viewportStart === 1 ? "" : "s"} above`));
  }
  let group: string | undefined;
  for (let i = s.viewportStart; i < end; i += 1) {
    const row = s.rows[i] as PickerRow;
    if (row.dependencyType !== group) {
      group = row.dependencyType;
      const count = s.rows.filter((r, j) => r.dependencyType === group && s.selected[j]).length;
      const label = count > 0 ? paint.bold(`${group} ${count}`) : group;
      const labelLen = group.length + (count > 0 ? 1 + String(count).length : 0);
      const pad = Math.max(1, 6 + nameW + 2 - (2 + labelLen));
      let header = `  ${label}${" ".repeat(pad)}Current${" ".repeat(curW - 7 + 2)}Target${" ".repeat(tgtW - 6 + 2)}Latest`;
      if (showWorkspace) header += `${" ".repeat(latW - 6 + 2)}Workspace`;
      lines.push("", header);
    }
    const selected = s.selected[i] === true;
    const target = targetOf(s, i);
    const color = selected ? colorOf(row.current, target) : undefined;
    const tint = (t: string) => (color ? paint[color](t) : t);
    const tag = tagOf(row.dependencyType);
    const name = truncate(row.name, Math.max(1, nameW - tag.length));
    let line = i === s.cursor ? `  ${paint.cyan("❯")} ` : "    ";
    line += `${tint(selected ? "■" : "□")} ${tint(name)}${tag ? paint.dim(tag) : ""}`;
    line += " ".repeat(Math.max(0, nameW - name.length - tag.length) + 2);
    line += row.current + " ".repeat(Math.max(0, curW - row.current.length) + 2);
    const latestPicked = selected && picksLatest(s, i);
    line += (selected && !latestPicked ? paint.underline(row.update) : row.update) + " ".repeat(Math.max(0, tgtW - row.update.length) + 2);
    const latest = latestPicked ? paint.underline(row.latest) : row.latest;
    line += row.latest === row.update ? paint.dim(latest) : latest;
    if (showWorkspace) {
      line += " ".repeat(Math.max(0, latW - row.latest.length) + 2) + paint.dim(row.workspace ?? "");
    }
    lines.push(line);
  }
  if (s.rows.length > s.viewportHeight && end < s.rows.length) {
    const n = s.rows.length - end;
    lines.push(paint.dim(`  ↓ ${n} more package${n === 1 ? "" : "s"} below`));
  }
  return lines;
}
