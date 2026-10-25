import { enableCompileCache, isEntryPoint, main } from "../entry.ts";

enableCompileCache();

// The entry check must live in the entry file itself: with code splitting,
// import.meta.url anywhere else names a shared chunk, not this bin.
if (process.argv[1] !== undefined && isEntryPoint(import.meta.url, process.argv[1])) {
  process.exitCode = await main("fnpm");
}
