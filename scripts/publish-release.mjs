#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: { dir: { type: 'string' }, publish: { type: 'boolean', default: false } } });
if (!values.dir) throw new Error('Usage: node scripts/publish-release.mjs --dir <packed-release-dir> [--publish]');
const directory = resolve(values.dir);
const release = JSON.parse(readFileSync(join(directory, 'packed.json'), 'utf8'));
if (!release.complete || release.packages.length !== 10) throw new Error('Publication requires all eight platform packages, the binding, and CLI');
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
const registry = 'https://registry.npmjs.org/';
for (const pkg of release.packages) {
  const bytes = readFileSync(pkg.tarball);
  const actual = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  if (actual !== pkg.integrity) throw new Error(`Tarball changed after packing: ${pkg.tarball}`);
}
if (values.publish) {
  const identity = spawnSync(npm, ['whoami', '--registry', registry], { encoding: 'utf8', shell: process.platform === 'win32' });
  if (identity.error || identity.status !== 0) throw new Error('npm authentication failed; run npm login locally before publishing');
  console.log(`Publishing as ${identity.stdout.trim()}`);
}
// Staging order is platforms -> binding -> CLI; publish dependents last.
for (const pkg of release.packages) {
  const args = ['publish', pkg.tarball, '--access', 'public', '--registry', registry, '--ignore-scripts'];
  if (!values.publish) args.push('--dry-run');
  const result = spawnSync(npm, args, { cwd: directory, stdio: 'inherit', shell: process.platform === 'win32' });
  if (result.error || result.status !== 0) throw new Error(`Publication stopped at ${pkg.name}: ${result.error?.message ?? result.status}`);
}
