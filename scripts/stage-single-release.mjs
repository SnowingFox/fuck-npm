#!/usr/bin/env node
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { spawnSync } from 'node:child_process';
import { TARGETS } from './stage-release.mjs';
import { checkBundle } from './check-bundle.mjs';
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const { values } = parseArgs({ options: { artifacts: { type: 'string' }, out: { type: 'string' }, name: { type: 'string', default: '@tiktok-fe/fnpm' }, registry: { type: 'string' }, 'asset-base': { type: 'string' }, target: { type: 'string' } } });
if (!values.artifacts || !values.out || !values.registry || !values['asset-base']) throw new Error('Usage: node scripts/stage-single-release.mjs --artifacts <dir> --out <new-dir> --registry <url> --asset-base <https-url> [--name <name>] [--target <smoke-target>]');
const output = resolve(values.out), artifacts = resolve(values.artifacts);
if (existsSync(output)) throw new Error('Use a new output directory');
if (!/^(@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$/.test(values.name)) throw new Error('Invalid package name');
const registry = new URL(values.registry), assetBase = new URL(values['asset-base']);
for (const url of [registry, assetBase]) if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) throw new Error('Invalid registry or asset URL');
if (assetBase.protocol !== 'https:' && !['127.0.0.1', 'localhost'].includes(assetBase.hostname)) throw new Error('Assets require HTTPS');
const source = join(ROOT, 'packages/fuck-npm');
const original = JSON.parse(readFileSync(join(source, 'package.json')));
checkBundle(source);
const targets = values.target ? TARGETS.filter(t => [t.target, t.abi].includes(values.target)) : TARGETS;
if (!targets.length) throw new Error('Unsupported target');
const inputs = targets.map(t => {
  const directory = join(artifacts, `binding-${t.target}`);
  const files = [`fnpm.${t.abi}.node`, `fnpm${t.os === 'win32' ? '.exe' : ''}`, `fnpm-lock${t.os === 'win32' ? '.exe' : ''}`];
  for (const file of files) if (!existsSync(join(directory, file))) throw new Error(`Missing artifact: ${t.abi}/${file}`);
  return { ...t, directory, files };
});
mkdirSync(output, { recursive: true });
const assetsDir = join(output, 'assets'); mkdirSync(assetsDir);
const assets = {};
for (const t of inputs) {
  const filename = `fnpm-v${original.version}-${t.abi}.tar.gz`;
  const archive = join(assetsDir, filename);
  const r = spawnSync('tar', ['-czf', archive, '-C', t.directory, ...t.files], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(r.stderr);
  const bytes = readFileSync(archive);
  assets[t.abi] = { url: new URL(filename, assetBase.href.replace(/\/?$/, '/')).href, integrity: `sha512-${createHash('sha512').update(bytes).digest('base64')}`, files: t.files, size: bytes.length };
}
const cli = join(output, 'cli'); mkdirSync(cli);
for (const entry of ['dist', 'bin', 'lib', 'scripts/postinstall.js', 'scripts/install-native.js']) cpSync(join(source, entry), join(cli, entry), { recursive: true });
for (const file of ['LICENSE', 'README.md']) cpSync(join(ROOT, file), join(cli, file));
const manifest = { name: values.name, version: original.version, description: original.description, type: 'module', license: 'MIT', engines: original.engines, bin: original.bin, files: ['dist', 'bin', 'lib', 'scripts/postinstall.js', 'scripts/install-native.js'], scripts: { postinstall: 'node scripts/postinstall.js' }, repository: { type: 'git', url: 'git+https://github.com/SnowingFox/fuck-npm.git' }, publishConfig: { access: 'public', registry: registry.href } };
writeFileSync(join(cli, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
writeFileSync(join(cli, 'lib/native-release.json'), JSON.stringify({ version: original.version, assets }, null, 2) + '\n');
writeFileSync(join(cli, 'lib/binding.cjs'), `'use strict';\nconst native=require('./native.cjs');\nconst path=native.findNativeBinding();\nif(!path)throw new Error('fnpm native files are missing; run node scripts/postinstall.js in this package or run fnpm once to install them');\nmodule.exports=require(path);\n`);
function rewrite(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) rewrite(path);
    else if (/\.(?:js|cjs)$/.test(entry.name)) {
      let text = readFileSync(path, 'utf8').replaceAll('"@fuck-npm/binding"', '"../lib/binding.cjs"').replaceAll('"fuck-npm"', JSON.stringify(values.name));
      // Dist chunks resolve the internal loader relative to dist. Source-only compatibility paths stay external.
      writeFileSync(path, text);
    }
  }
}
rewrite(join(cli, 'dist'));
checkBundle(cli);
const release = { name: manifest.name, version: manifest.version, registry: registry.href, complete: !values.target, distribution: 'single', packages: [{ name: manifest.name, version: manifest.version, directory: cli }], assets };
writeFileSync(join(output, 'release.json'), JSON.stringify(release, null, 2) + '\n');
console.log(`Staged ${manifest.name}@${manifest.version}: one npm package, ${inputs.length} native archives`);
