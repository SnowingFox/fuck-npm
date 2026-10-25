"""Turns the recorder output into the fixtures of fnpm-semver/tests.

Usage (from a scratch dir holding bun's semver.test.ts, semver-fixture.js,
__snapshots__/semver.test.ts.snap and node_modules/semver@7.7.4):
    bun run record.ts out.json && python3 export.py <crate>/tests/fixtures
record.ts is semver.test.ts with its imports pointed at shim.ts and Bun.semver
replaced by semverShim, plus a final `finish(process.argv[2])`.
"""
import json, re, sys

out = sys.argv[1]
lines = []
for r in json.load(open("out.json")):
    # String(Symbol()) works in JS; Bun.semver throws on it. Not a semver case.
    if r["f"] == "satisfies" and r["e"] == "throw":
        continue
    lines.append(json.dumps([r["f"], r["a"], r["b"], r["e"]], ensure_ascii=True, separators=(",", ":")))
open(out + "/bun-semver-test.jsonl", "w").write("\n".join(lines) + "\n")
snap = open("semver.test.ts.snap").read()
m = re.search(r"exports\[`[^`]*pre-release snapshot[^`]*`\] = `\n\[\n(.*?)\n\]\n`;", snap, re.S)
items = re.findall(r'^\s+"(.*)",$', m.group(1), re.M)
open(out + "/bun-prerelease-order.txt", "w").write("\n".join(items) + "\n")
unsorted = re.findall(r'^\s+"(.*)",?\s*$', open("semver-fixture.js").read(), re.M)
open(out + "/bun-prerelease-unsorted.txt", "w").write("\n".join(unsorted) + "\n")
print(len(lines), len(items), len(unsorted))
