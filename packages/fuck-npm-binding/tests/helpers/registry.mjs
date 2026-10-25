// A minimal npm registry on node:http for end-to-end tests of the binding: full packuments
// and gzipped tarballs built in memory, everything else 404. The shared registry in
// tests/registry needs Node 24 and bun's fixtures; this one runs anywhere the binding does.
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { gzipSync } from "node:zlib";

/** A ustar archive of `files` under `package/`, gzipped. */
export function tarball(files) {
  const blocks = [];
  for (const [name, content] of Object.entries(files)) {
    const body = Buffer.from(content);
    const header = Buffer.alloc(512);
    header.write(`package/${name}`, 0, 100);
    header.write("0000644\0", 100);
    header.write("0000000\0", 108);
    header.write("0000000\0", 116);
    header.write(`${body.length.toString(8).padStart(11, "0")}\0`, 124);
    header.write("00000000000\0", 136);
    header.write("        ", 148);
    header.write("0", 156);
    header.write("ustar\0", 257);
    header.write("00", 263);
    let sum = 0;
    for (const byte of header) sum += byte;
    header.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148);
    blocks.push(header, body, Buffer.alloc((512 - (body.length % 512)) % 512));
  }
  blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks));
}

/**
 * Serves `packages` (`{ name: { version: { dependencies?, files? } } }`) on 127.0.0.1.
 * Resolves with `{ url, requests, close() }`; `url` ends with a slash.
 */
export async function startRegistry(packages) {
  const tarballs = new Map();
  const requests = [];
  const server = createServer((req, res) => {
    requests.push(req.url);
    const path = decodeURIComponent(req.url.split("?")[0]).slice(1);
    const tgz = tarballs.get(path);
    if (tgz) {
      res.writeHead(200, { "content-type": "application/octet-stream", "content-length": tgz.length });
      res.end(tgz);
      return;
    }
    const versions = packages[path];
    if (!versions) {
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "Not found" }));
      return;
    }
    const doc = { name: path, "dist-tags": {}, versions: {} };
    for (const [version, spec] of Object.entries(versions)) {
      const manifest = { name: path, version, dependencies: spec.dependencies ?? {} };
      const file = `${path}/-/${path.split("/").pop()}-${version}.tgz`;
      const tgz = tarball({
        "package.json": JSON.stringify(manifest),
        "index.js": `module.exports = ${JSON.stringify(`${path}@${version}`)};\n`,
        ...spec.files,
      });
      tarballs.set(file, tgz);
      doc.versions[version] = {
        ...manifest,
        dist: {
          tarball: `${url}${file}`,
          shasum: createHash("sha1").update(tgz).digest("hex"),
          integrity: `sha512-${createHash("sha512").update(tgz).digest("base64")}`,
        },
      };
      doc["dist-tags"].latest = version;
    }
    const body = JSON.stringify(doc);
    res.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
    res.end(body);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}/`;
  return {
    url,
    requests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
