// The security scanner's process (bun `install/PackageManager/scanner-entry.ts`), run with
// `node --input-type=module -e`. The core substitutes the placeholders: the scanner module, whether
// to hide the scanner's own errors, and the files it reads the packages from and writes the
// answer to (bun uses fds 3 and 4; files work the same on every platform).
import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const scannerModuleName = __SCANNER_MODULE__;
const suppressError = __SUPPRESS_ERROR__;
const inputPath = __INPUT_PATH__;
const outputPath = __OUTPUT_PATH__;

// Scanners written for bun may call these.
if (typeof globalThis.Bun === "undefined") {
  globalThis.Bun = { sleep: ms => new Promise(resolve => setTimeout(resolve, ms)) };
}

function sendAndExit(message) {
  fs.writeFileSync(outputPath, JSON.stringify(message));
  process.exit(message.type === "error" ? 1 : 0);
}

let packages;
try {
  packages = JSON.parse(fs.readFileSync(inputPath, "utf8"));
  if (!Array.isArray(packages)) {
    throw new Error("Expected packages to be an array");
  }
} catch (error) {
  sendAndExit({
    type: "error",
    code: "SCAN_FAILED",
    message: `Failed to parse packages JSON: ${error instanceof Error ? error.message : String(error)}`,
  });
}

const isPath =
  scannerModuleName.startsWith(".") || scannerModuleName.startsWith("/") || path.isAbsolute(scannerModuleName);
const specifier = isPath ? pathToFileURL(path.resolve(process.cwd(), scannerModuleName)).href : scannerModuleName;

let scanner;
try {
  const mod = await import(specifier);
  scanner = mod.scanner ?? mod.default?.scanner;
} catch (error) {
  if (typeof error === "object" && error !== null && "code" in error && error.code === "ERR_MODULE_NOT_FOUND") {
    if (!suppressError) {
      console.error(`\x1b[31merror: \x1b[0mFailed to import security scanner: \x1b[1m'${scannerModuleName}'`);
    }
    sendAndExit({ type: "error", code: "MODULE_NOT_FOUND", module: scannerModuleName });
  } else {
    sendAndExit({
      type: "error",
      code: "SCAN_FAILED",
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

try {
  if (typeof scanner !== "object" || scanner === null || typeof scanner.version !== "string") {
    throw new Error("Security scanner must export a 'scanner' object with a version property");
  }
  if (scanner.version !== "1") {
    sendAndExit({
      type: "error",
      code: "INVALID_VERSION",
      message: `Security scanner must be version 1, got version ${scanner.version}`,
    });
  }
  if (typeof scanner.scan !== "function") {
    throw new Error(`scanner.scan is not a function, got ${typeof scanner.scan}`);
  }
  const result = await scanner.scan({ packages });
  if (!Array.isArray(result)) {
    throw new Error("Security scanner must return an array of advisories");
  }
  sendAndExit({ type: "result", advisories: result });
} catch (error) {
  if (!suppressError) {
    console.error(error);
  }
  sendAndExit({
    type: "error",
    code: "SCAN_FAILED",
    message: error instanceof Error ? error.message : "Unknown error occurred",
  });
}
