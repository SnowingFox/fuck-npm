#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

export function publishRelease({ dir, cli = 'npm', registry, publish = false }, run = spawnSync) {
  if (!['npm', 'bnpm'].includes(cli)) throw new Error('--cli must be npm or bnpm');
  const directory = resolve(dir);
  const release = JSON.parse(readFileSync(join(directory, 'packed.json'), 'utf8'));
  if (!release.complete || release.packages.length !== 10) throw new Error('Publication requires all eight platform packages, the binding, and CLI');
  const stagedRegistry = release.registry ?? 'https://registry.npmjs.org/';
  const registryUrl = new URL(registry ?? stagedRegistry);
  if (!['http:', 'https:'].includes(registryUrl.protocol) || registryUrl.username || registryUrl.password || registryUrl.search || registryUrl.hash) {
    throw new Error('--registry must be an HTTP(S) URL without credentials, query, or fragment');
  }
  registry = `${registryUrl.href.replace(/\/+$/, '')}/`;
  if (registry !== `${new URL(stagedRegistry).href.replace(/\/+$/, '')}/`) {
    throw new Error('Registry differs from staged publishConfig; stage and pack a new release with --registry');
  }
  const command = process.platform === 'win32' ? `${cli}.cmd` : cli;
  const options = { cwd: directory, encoding: 'utf8', shell: process.platform === 'win32' };
  // Verify the complete input set before even running a dry-run command.
  for (const pkg of release.packages) {
    const bytes = readFileSync(pkg.tarball);
    const actual = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
    if (actual !== pkg.integrity) throw new Error(`Tarball changed after packing: ${pkg.tarball}`);
  }
  if (publish) {
    const identity = run(command, ['whoami', '--registry', registry], options);
    if (identity.error || identity.status !== 0) throw new Error(`${cli} authentication failed; run ${cli} login --registry ${registry} locally before publishing`);
    console.log(`Publishing as ${identity.stdout.trim()} to ${registry} using ${cli}`);
  }
  // Staging order is platforms -> binding -> CLI; publish dependents last.
  for (const pkg of release.packages) {
    if (publish) {
      const existing = run(command, ['view', `${pkg.name}@${pkg.version}`, 'dist.integrity', '--json', '--registry', registry], options);
      if (existing.error) throw existing.error;
      if (existing.status === 0) {
        const integrity = JSON.parse(existing.stdout);
        if (integrity !== pkg.integrity) throw new Error(`Registry ${pkg.name}@${pkg.version} exists with different integrity; refusing to continue`);
        console.log(`Already published with matching integrity: ${pkg.name}@${pkg.version}`);
        continue;
      }
      let error;
      try { error = JSON.parse(existing.stdout).error; } catch {}
      if (error?.code !== 'E404') throw new Error(`Cannot check registry version for ${pkg.name}: ${existing.stderr}`);
    }
    const args = ['publish', pkg.tarball, '--access', 'public', '--registry', registry, '--ignore-scripts'];
    if (!publish) args.push('--dry-run');
    const result = run(command, args, { ...options, stdio: 'inherit' });
    if (result.error || result.status !== 0) throw new Error(`Publication stopped at ${pkg.name}: ${result.error?.message ?? result.status}`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const { values } = parseArgs({ options: {
      dir: { type: 'string' }, cli: { type: 'string', default: 'npm' }, registry: { type: 'string' },
      publish: { type: 'boolean', default: false },
    } });
    if (!values.dir) throw new Error('Usage: node scripts/publish-release.mjs --dir <packed-release-dir> [--cli npm|bnpm] [--registry <url>] [--publish]');
    publishRelease(values);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
