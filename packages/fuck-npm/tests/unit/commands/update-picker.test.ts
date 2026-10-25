import { describe, expect, test } from "vitest";
import { colorOf, initialState, press, render, targetOf, type PickerRow } from "@/commands/update-picker.ts";

const rows: PickerRow[] = [
  { name: "b", current: "1.0.0", update: "1.0.1", latest: "2.0.0", dependencyType: "devDependencies" },
  { name: "a", current: "1.0.0", update: "1.1.0", latest: "1.1.0", dependencyType: "dependencies" },
  { name: "c", current: "1.0.0", update: "1.0.0", latest: "1.2.0", dependencyType: "dependencies" },
];

const keys = (s: ReturnType<typeof initialState>, input: string) => {
  let rest = input;
  const results: string[] = [];
  while (rest.length > 0) {
    const { result, used } = press(s, rest);
    if (used === 0) break;
    results.push(result);
    rest = rest.slice(used);
  }
  return results;
};

const identity = (t: string) => t;
const paint = { bold: identity, dim: identity, cyan: identity, red: identity, yellow: identity, green: identity, underline: identity };

describe("update -i picker", () => {
  test("rows are grouped dependencies first, then by name", () => {
    const s = initialState(rows, 40, false);
    expect(s.rows.map((r) => r.name)).toEqual(["a", "c", "b"]);
  });

  test("space, a, n, i, l and the arrows", () => {
    const s = initialState(rows, 40, false);
    keys(s, " ");
    expect(s.selected).toEqual([true, false, false]);
    keys(s, "j ");
    // c has nothing newer in range, so selecting it picks latest.
    expect(targetOf(s, 1)).toBe("1.2.0");
    keys(s, "n");
    expect(s.selected).toEqual([false, false, false]);
    keys(s, "i");
    expect(s.selected).toEqual([true, true, true]);
    keys(s, "\x1b[B\x1b[C");
    expect(s.cursor).toBe(2);
    expect(targetOf(s, 2)).toBe("2.0.0");
    keys(s, "\x1b[D");
    expect(targetOf(s, 2)).toBe("1.0.1");
    keys(s, "\x1b[A\x1b[A\x1b[A");
    expect(s.cursor).toBe(2);
    expect(keys(s, "\r")).toEqual(["confirm"]);
    expect(keys(s, "\x03")).toEqual(["cancel"]);
  });

  test("l right after a flips every selected row", () => {
    const s = initialState(rows, 40, false);
    keys(s, "al");
    expect(s.rows.map((_, i) => targetOf(s, i))).toEqual(["1.1.0", "1.2.0", "2.0.0"]);
  });

  test("an incomplete escape sequence waits for more input", () => {
    const s = initialState(rows, 40, false);
    expect(press(s, "\x1b[")).toEqual({ result: "noop", used: 0 });
    expect(press(s, "\x1b[5~").used).toBe(4);
    expect(press(s, "\x1b[<65;3;4M").used).toBe(10);
  });

  test("colors follow the size of the move", () => {
    expect(colorOf("1.0.0", "2.0.0")).toBe("red");
    expect(colorOf("1.0.0", "1.1.0")).toBe("yellow");
    expect(colorOf("1.0.0", "1.0.1")).toBe("green");
    expect(colorOf("0.1.0", "0.2.0")).toBe("red");
    expect(colorOf("0.0.1", "0.0.2")).toBe("red");
  });

  test("render draws the group headers and a cursor", () => {
    const s = initialState(rows, 40, false);
    keys(s, " ");
    const lines = render(s, 120, paint);
    expect(lines[0]).toContain("Select packages to update");
    expect(lines.some((l) => /^ {2}dependencies 1 +Current {2}Target {2}Latest$/.test(l))).toBe(true);
    expect(lines.some((l) => l.startsWith("  ❯ ■ a"))).toBe(true);
    expect(lines.some((l) => l.includes("□ b dev"))).toBe(true);
  });
});
