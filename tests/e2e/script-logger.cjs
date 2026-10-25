// Preloaded into every node process during an e2e install (NODE_OPTIONS=--require).
// When the process is a dependency lifecycle script (the package manager sets
// npm_lifecycle_event), append one JSON line to $FNPM_E2E_SCRIPT_LOG describing it.
// Scripts that never start node (pure shell) are not seen here; run.ts complements this
// with a static scan of install scripts in the tree.
"use strict";
const event = process.env.npm_lifecycle_event;
const logFile = process.env.FNPM_E2E_SCRIPT_LOG;
const LIFECYCLE = new Set(["preinstall", "install", "postinstall", "preprepare", "prepare", "postprepare"]);
if (event && logFile && LIFECYCLE.has(event)) {
  try {
    const fs = require("node:fs");
    const path = require("node:path");
    let name = process.env.npm_package_name;
    let version = process.env.npm_package_version;
    if (!name) {
      try {
        const pkg = JSON.parse(fs.readFileSync(path.join(process.cwd(), "package.json"), "utf8"));
        name = pkg.name;
        version = pkg.version;
      } catch {}
    }
    fs.appendFileSync(logFile, JSON.stringify({ event, name: name ?? null, version: version ?? null, cwd: process.cwd() }) + "\n");
  } catch {}
}
