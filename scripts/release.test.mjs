import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { afterEach, test } from 'node:test';
import { stageRelease, TARGETS } from './stage-release.mjs';
import { publishRelease } from './publish-release.mjs';

const roots = [];
afterEach(() => { for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true }); });
const json = (file) => JSON.parse(readFileSync(file, 'utf8'));
function fixture(registry = 'https://registry.example.com/') {
  const root = mkdtempSync(join(tmpdir(), 'fnpm-release-test-'));
  roots.push(root);
  const put = (file, text) => { const path = join(root, file); mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, text); };
  const bins = Object.fromEntries(['fnpm', 'fnpm-lock', 'fnpmx', 'fnpx'].map((bin) => [bin, `./bin/${bin}`]));
  put('packages/fuck-npm/package.json', JSON.stringify({ version: '0.0.1', bin: bins, files: ['dist', 'bin', 'lib', 'scripts/postinstall.js'], scripts: { postinstall: 'node scripts/postinstall.js' } }));
  put('packages/fuck-npm-binding/package.json', '{"version":"0.0.1"}');
  put('packages/fuck-npm-binding/index.js', 'require("@fuck-npm/binding-linux-x64-gnu");');
  put('packages/fuck-npm-binding/index.d.ts', '');
  put('LICENSE', 'MIT');
  put('README.md', 'fnpm');
  for (const bin of Object.keys(bins)) {
    put(`packages/fuck-npm/dist/${bin}.js`, '');
    put(`packages/fuck-npm/bin/${bin}`, '#!/usr/bin/env node\n');
  }
  put('packages/fuck-npm/lib/native.cjs', '');
  put('packages/fuck-npm/scripts/postinstall.js', '');
  for (const item of TARGETS) {
    const suffix = item.os === 'win32' ? '.exe' : '';
    for (const file of [`fnpm.${item.abi}.node`, `fnpm${suffix}`, `fnpm-lock${suffix}`]) put(`artifacts/${item.target}/${file}`, 'artifact');
  }
  const dir = join(root, 'release');
  const release = stageRelease({ name: '@snowingfox/fuck-npm', registry, artifacts: join(root, 'artifacts'), out: dir, root });
  for (const [index, pkg] of release.packages.entries()) {
    pkg.tarball = join(root, `${index}.tgz`);
    const bytes = Buffer.from(pkg.name);
    writeFileSync(pkg.tarball, bytes);
    pkg.integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  }
  writeFileSync(join(dir, 'packed.json'), JSON.stringify(release));
  return { root, dir, release };
}

test('bnpm staging carries the registry and fnpx through all leaf packages', () => {
  const { dir, release } = fixture();
  assert.equal(release.registry, 'https://registry.example.com/');
  assert.equal(release.packages.length, 10);
  for (const pkg of release.packages) assert.equal(json(join(pkg.directory, 'package.json')).publishConfig.registry, release.registry);
  const cli = json(join(dir, 'cli/package.json'));
  assert.equal(cli.bin.fnpx, './bin/fnpx');
  assert.equal(cli.bin.fnpmx, './bin/fnpmx');
  assert.deepEqual(cli.dependencies, { '@snowingfox/fnpm-binding': '0.0.1' });
});

test('bnpm dry-run uses the selected CLI, registry, and packed leaf tarballs', () => {
  const { dir, release } = fixture();
  const calls = [];
  publishRelease({ dir, cli: 'bnpm' }, (command, args) => { calls.push({ command, args }); return { status: 0 }; });
  assert.equal(calls.length, 10);
  for (const [index, call] of calls.entries()) {
    assert.equal(call.command, process.platform === 'win32' ? 'bnpm.cmd' : 'bnpm');
    assert.deepEqual(call.args, ['publish', release.packages[index].tarball, '--access', 'public', '--registry', release.registry, '--ignore-scripts', '--dry-run']);
  }
});

test('publication resumes matching versions and publishes missing packages with bnpm', () => {
  const { dir, release } = fixture();
  const published = [];
  let viewed = 0;
  publishRelease({ dir, cli: 'bnpm', publish: true }, (command, args) => {
    assert.equal(command, process.platform === 'win32' ? 'bnpm.cmd' : 'bnpm');
    assert.equal(args[args.indexOf('--registry') + 1], release.registry);
    if (args[0] === 'whoami') return { status: 0, stdout: 'release-user\n' };
    if (args[0] === 'view') return viewed++ === 0
      ? { status: 0, stdout: JSON.stringify(release.packages[0].integrity) }
      : { status: 1, stdout: '{"error":{"code":"E404"}}' };
    published.push(args);
    return { status: 0 };
  });
  assert.deepEqual(published.map((args) => args[1]), release.packages.slice(1).map((pkg) => pkg.tarball));
  assert.ok(published.every((args) => !args.includes('--dry-run')));
});

test('registry mismatch and changed tarballs fail before invoking the publish CLI', () => {
  const { dir, release } = fixture();
  const never = () => { assert.fail('publish CLI must not run'); };
  assert.throws(() => publishRelease({ dir, cli: 'bnpm', registry: 'https://registry.npmjs.org/' }, never), /Registry differs/);
  writeFileSync(release.packages.at(-1).tarball, 'changed');
  assert.throws(() => publishRelease({ dir, cli: 'bnpm', publish: true }, never), /Tarball changed/);
});

test('conflicting published integrity stops before publishing another package', () => {
  const { dir } = fixture();
  assert.throws(() => publishRelease({ dir, cli: 'bnpm', publish: true }, (_command, args) => {
    if (args[0] === 'whoami') return { status: 0, stdout: 'release-user' };
    assert.equal(args[0], 'view');
    return { status: 0, stdout: '"sha512-other"' };
  }), /different integrity/);
});
