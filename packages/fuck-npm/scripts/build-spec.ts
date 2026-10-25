import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, relative } from "node:path";
import { renderSpec, SPEC_PATH } from "./cli-spec.ts";

/** `npm run build:spec`: write crates/fnpm-cli/cli-spec.json from the TS command tree. */
mkdirSync(dirname(SPEC_PATH), { recursive: true });
writeFileSync(SPEC_PATH, renderSpec());
process.stdout.write(`wrote ${relative(process.cwd(), SPEC_PATH)}\n`);
