#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createRequire } from 'node:module';

const directory = process.argv[2] && resolve(process.argv[2]);
if (!directory) throw new Error('Usage: node scripts/smoke-release.mjs <packed-release-dir>');
const release = JSON.parse(readFileSync(join(directory, 'packed.json'), 'utf8'));
const packages = new Map(release.packages.map((pkg) => [pkg.name, pkg]));
const scratch = mkdtempSync(join(tmpdir(), 'fnpm-release-smoke-'));
const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
const server = createServer((req, res) => {
  try {
    const path = decodeURIComponent(req.url.split('?')[0]).slice(1);
    const tar = /^tarball\/(\d+)$/.exec(path);
    if (tar) {
      const pkg = release.packages[Number(tar[1])];
      res.end(readFileSync(pkg.tarball));
      return;
    }
    const pkg = packages.get(path);
    if (!pkg) { res.writeHead(404); res.end('{}'); return; }
    const manifest = JSON.parse(readFileSync(join(pkg.directory, 'package.json'), 'utf8'));
    const meta = { ...manifest, dist: {
      tarball: `http://127.0.0.1:${server.address().port}/tarball/${release.packages.indexOf(pkg)}`,
      integrity: pkg.integrity, shasum: pkg.shasum,
    } };
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ name: pkg.name, 'dist-tags': { latest: release.version }, versions: { [release.version]: meta } }));
  } catch (error) { res.writeHead(500); res.end(error.message); }
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const registry = `http://127.0.0.1:${server.address().port}`;
const env = { ...process.env,
  npm_config_cache: join(scratch, 'npm-cache'), npm_config_userconfig: join(scratch, 'npmrc'),
  npm_config_registry: registry, NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost',
};
// Installed-package smoke must not resolve a development override.
delete env.NAPI_RS_NATIVE_LIBRARY_PATH;
delete env.FNPM_JS_ENTRY;
delete env.FNPM_LOCK_JS_ENTRY;
writeFileSync(env.npm_config_userconfig, '');
async function run(command, args, cwd, extraEnv = {}) {
  const child = spawn(command, args, { cwd, env: { ...env, ...extraEnv }, shell: process.platform === 'win32' && command === npm });
  let stdout = '', stderr = '';
  child.stdout.on('data', (data) => { stdout += data; });
  child.stderr.on('data', (data) => { stderr += data; });
  const timer = setTimeout(() => child.kill(), 120_000);
  try {
    const code = await new Promise((done, fail) => { child.on('error', fail); child.on('exit', done); });
    assert.equal(code, 0, `${command} ${args.join(' ')} failed\n${stdout}\n${stderr}`);
    return stdout;
  } finally { clearTimeout(timer); }
}
try {
  for (const ignoreScripts of [false, true]) {
    const root = join(scratch, ignoreScripts ? 'ignore-scripts' : 'normal');
    mkdirSync(root);
    writeFileSync(join(root, 'package.json'), JSON.stringify({ name: 'release-smoke', private: true, allowScripts: { [release.name]: true } }));
    await run(npm, ['install', `${release.name}@${release.version}`, '--no-audit', '--no-fund', ...(ignoreScripts ? ['--ignore-scripts'] : [])], root);
    const cli = join(root, 'node_modules', release.name);
    await run(process.execPath, ['-e', `const {createRequire}=require('node:module'); const binding=createRequire(${JSON.stringify(join(cli, 'package.json'))})(${JSON.stringify(release.bindingName)}); if(typeof binding.install!=='function')process.exit(1);`], root);
    const native = createRequire(join(cli, 'package.json'))('./lib/native.cjs');
    const fixture = join(root, 'project');
    mkdirSync(fixture);
    writeFileSync(join(fixture, 'package.json'), '{"name":"release-project","private":true}');
    const bins = Object.fromEntries(['fnpm', 'fnpm-lock', 'fnpmx', 'fnpx'].map((bin) => [bin, join(cli, 'bin', bin)]));
    const invoke = (bin, args, cwd = fixture) => process.platform === 'win32'
      ? run(process.execPath, [bins[bin], ...args], cwd) : run(bins[bin], args, cwd);
    for (const bin of ['fnpm', 'fnpm-lock']) {
      assert.equal((await invoke(bin, ['--version'])).trim(), release.version);
      assert.match(await invoke(bin, ['--help']), /Usage:/i);
    }
    for (const alias of ['fnpmx', 'fnpx']) {
      await invoke(alias, ['--help']);
    }
    await invoke('fnpm', ['install', '--offline', '--ignore-scripts']);
    await invoke('fnpm', ['install', '--offline', '--ignore-scripts']);
    await invoke('fnpm', ['ci', '--offline', '--ignore-scripts']);
    // Exercise dependency materialization without relying on an external registry.
    const dep = join(root, 'smoke-dependency');
    mkdirSync(dep);
    writeFileSync(join(dep, 'package.json'), '{"name":"fnpm-release-fixture","version":"1.0.0","main":"index.js","bin":{"release-fixture":"cli.js"}}');
    writeFileSync(join(dep, 'index.js'), 'module.exports = 42;\n');
    writeFileSync(join(dep, 'cli.js'), '#!/usr/bin/env node\nconsole.log(42);\n');
    chmodSync(join(dep, 'cli.js'), 0o755);
    const packed = JSON.parse(await run(npm, ['pack', '--json', '--ignore-scripts'], dep));
    await invoke('fnpm', ['add', join(dep, packed[0].filename), '--ignore-scripts']);
    assert.equal(createRequire(join(fixture, 'package.json'))('fnpm-release-fixture'), 42);
    for (const alias of ['fnpmx', 'fnpx']) {
      assert.equal((await invoke(alias, ['--no-install', 'release-fixture'])).trim(), '42');
    }
    await invoke('fnpm', ['install', '--offline', '--ignore-scripts']);
    rmSync(join(fixture, 'node_modules'), { recursive: true });
    await invoke('fnpm', ['ci', '--offline', '--ignore-scripts']);
    assert.equal(createRequire(join(fixture, 'package.json'))('fnpm-release-fixture'), 42);
    if (process.platform !== 'win32') {
      const signature = readFileSync(bins.fnpm).subarray(0, 2).toString();
      assert.equal(signature === '#!', ignoreScripts, 'normal npm install must use native executable, --ignore-scripts must retain shim');
    }
    console.log(`${ignoreScripts ? '--ignore-scripts' : 'normal'} npm tarball install: versions, help, addon, offline/no-op/frozen/dependency install passed (${native.platformArchAbi()})`);
  }
} finally {
  await new Promise((done) => server.close(done));
  rmSync(scratch, { recursive: true, force: true });
}
