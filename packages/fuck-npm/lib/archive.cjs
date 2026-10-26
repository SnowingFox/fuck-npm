'use strict';
const { spawnSync } = require('node:child_process');
const { mkdirSync, writeFileSync } = require('node:fs');
const { dirname, isAbsolute, join, relative } = require('node:path');
const { gunzipSync } = require('node:zlib');

function nativeVersion(exe) {
  const result = spawnSync(exe, ["--version"], { encoding: "utf8", timeout: 10_000, windowsHide: true });
  if (result.error || result.status !== 0) {
    return null;
  }
  return result.stdout.trim();
}

function extractFiles(tgz, destination, wanted) {
  const buffer = gunzipSync(tgz);
  const field = (offset, length) =>
    buffer.subarray(offset, offset + length).toString("utf8").replace(/\0.*$/s, "");
  const written = [];
  let offset = 0;
  while (offset + 512 <= buffer.length) {
    const name = field(offset, 100);
    if (name === "") break;
    const prefix = field(offset + 345, 155);
    const size = Number.parseInt(field(offset + 124, 12).trim() || "0", 8);
    const type = field(offset + 156, 1);
    offset += 512;
    if (!Number.isSafeInteger(size) || size < 0 || size > buffer.length - offset) throw new Error("Invalid native archive entry size");
    const full = (prefix ? `${prefix}/${name}` : name).replace(/^package\//, "");
    if ((type === "" || type === "0") && wanted.includes(full)) {
      const target = join(destination, full);
      const inside = relative(destination, target);
      if (inside && !inside.startsWith("..") && !isAbsolute(inside)) {
        mkdirSync(dirname(target), { recursive: true });
        writeFileSync(target, buffer.subarray(offset, offset + size));
        written.push(full);
      }
    }
    offset += Math.ceil(size / 512) * 512;
  }
  return written;
}

module.exports = { nativeVersion, extractFiles };
