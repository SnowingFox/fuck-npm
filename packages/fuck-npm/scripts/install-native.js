// Single-package releases download only the current platform's verified native archive.
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, chmodSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
const require = createRequire(import.meta.url);
const native = require('../lib/native.cjs');
const { extractFiles, nativeVersion } = require('../lib/archive.cjs');

export async function installNative({ root = native.PACKAGE_ROOT, abi = native.platformArchAbi(), fetchImpl = fetch, env = process.env } = {}) {
  const manifest = JSON.parse(readFileSync(join(root, 'lib/native-release.json'), 'utf8'));
  const asset = manifest.assets[abi];
  if (!asset) throw new Error(`No fnpm native build for ${abi ?? `${process.platform}-${process.arch}`}`);
  const destination = join(root, 'native', abi);
  const valid = () => asset.files.every(file => existsSync(join(destination, file))) && native.BINS.every(bin => nativeVersion(join(destination, native.executableName(bin))) === manifest.version);
  if (valid()) return destination;
  const mirror = env.FNPM_NATIVE_MIRROR;
  const url = mirror ? new URL(new URL(asset.url).pathname.split('/').at(-1), mirror.replace(/\/?$/, '/')) : new URL(asset.url);
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname))) throw new Error('Native archive URL must use HTTPS');
  process.stderr.write(`fnpm: downloading native build for ${abi}\n`);
  const response = await fetchImpl(url);
  if (!response.ok) throw new Error(`Native download failed: HTTP ${response.status} (${url})`);
  const bytes = Buffer.from(await response.arrayBuffer());
  const integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  if (integrity !== asset.integrity) throw new Error('Native archive integrity check failed');
  mkdirSync(join(root, 'native'), { recursive: true });
  const staging = mkdtempSync(join(root, 'native', '.install-'));
  try {
    const written = extractFiles(bytes, staging, asset.files);
    if (!asset.files.every(file => written.includes(file))) throw new Error('Native archive is incomplete');
    for (const bin of native.BINS) chmodSync(join(staging, native.executableName(bin)), 0o755);
    if (!native.BINS.every(bin => nativeVersion(join(staging, native.executableName(bin))) === manifest.version)) throw new Error('Native executable version does not match this package');
    if (valid()) return destination;
    // Replace an incomplete previous attempt only after the complete archive passes validation.
    if (existsSync(destination)) rmSync(destination, { recursive: true });
    try { renameSync(staging, destination); }
    catch (error) { if (!valid()) throw error; }
    return destination;
  } finally { rmSync(staging, { recursive: true, force: true }); }
}
