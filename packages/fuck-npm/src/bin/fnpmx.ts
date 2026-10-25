import { enableCompileCache, isEntryPoint, main } from "../entry.ts";

enableCompileCache();

// See bin/fnpm.ts: the check has to sit in the entry file, not a shared chunk.
if (process.argv[1] !== undefined && isEntryPoint(import.meta.url, process.argv[1])) {
  process.exitCode = await main("fnpmx");
}
