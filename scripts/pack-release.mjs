#!/usr/bin/env node
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { checkBundle } from './check-bundle.mjs';

const directory = process.argv[2] && resolve(process.argv[2]);
if (!directory) throw new Error('Usage: node scripts/pack-release.mjs <staged-release-dir>');
const release = JSON.parse(readFileSync(join(directory, 'release.json'), 'utf8'));
const tarballs = join(directory, 'tarballs');
mkdirSync(tarballs); // Refuse to mix a new release with older tarballs.
const packed = [];
for (const pkg of release.packages) {
  const result = spawnSync(process.platform === 'win32' ? 'npm.cmd' : 'npm', [
    'pack', '--json', '--ignore-scripts', '--workspaces=false', '--pack-destination', tarballs,
  ], { cwd: pkg.directory, encoding: 'utf8', shell: process.platform === 'win32' });
  if (result.error || result.status !== 0) throw new Error(result.error?.message ?? result.stderr);
  const [info] = JSON.parse(result.stdout);
  if (info.name !== pkg.name || info.version !== release.version) throw new Error(`Wrong package identity: ${pkg.name}`);
  const files = info.files.map((file) => file.path);
  const manifest = JSON.parse(readFileSync(join(pkg.directory, 'package.json'), 'utf8'));
  const required = pkg.name === release.name
    ? ['dist/fnpm.js', 'dist/fnpm-lock.js', 'dist/fnpmx.js', 'dist/fnpx.js', 'bin/fnpm', 'bin/fnpm-lock', 'bin/fnpmx', 'bin/fnpx', 'bin/package.json', 'lib/native.cjs', 'lib/shim.cjs', 'scripts/postinstall.js']
    : pkg.name === release.bindingName ? ['index.js', 'index.d.ts'] : manifest.files;
  for (const file of ['package.json', 'LICENSE', ...required]) {
    if (!files.includes(file)) throw new Error(`${pkg.name} tarball is missing ${file}`);
  }
  if (pkg.name === release.name) {
    checkBundle(pkg.directory, files);
    if (release.distribution === 'single') {
      if (manifest.dependencies || manifest.optionalDependencies) throw new Error('Single-package releases must not depend on platform npm packages');
      for (const file of ['lib/native-release.json', 'lib/binding.cjs', 'lib/archive.cjs', 'scripts/install-native.js']) {
        if (!files.includes(file)) throw new Error(`Single package release is missing ${file}`);
      }
    }
  }
  packed.push({ ...pkg, tarball: join(tarballs, info.filename), integrity: info.integrity, shasum: info.shasum, size: info.size, files });
  console.log(`${pkg.name}@${pkg.version}: ${info.size} bytes, ${files.length} files`);
}
writeFileSync(join(directory, 'packed.json'), `${JSON.stringify({ ...release, packages: packed }, null, 2)}\n`);
