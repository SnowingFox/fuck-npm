// Usage: node threads.cjs <binding dir>. Linux only: prints how many threads the process
// has before and after loading the addon, and after a synchronous call into it.
const { readdirSync } = require("node:fs");
const threads = () => readdirSync("/proc/self/task").length;
const before = threads();
const binding = require(process.argv[2]);
const loaded = threads();
binding.version();
process.stdout.write(JSON.stringify({ before, loaded, afterVersion: threads() }));
