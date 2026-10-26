import { enableCompileCache, isEntryPoint, main } from "../entry.ts";

enableCompileCache();

if (process.argv[1] !== undefined && isEntryPoint(import.meta.url, process.argv[1])) {
  process.exitCode = await main("fnpx");
}
