#!/usr/bin/env node
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync, chmodSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { checkBundle } from './check-bundle.mjs';

export const TARGETS = [
  { target: 'aarch64-apple-darwin', abi: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
  { target: 'x86_64-apple-darwin', abi: 'darwin-x64', os: 'darwin', cpu: 'x64' },
  { target: 'x86_64-unknown-linux-gnu', abi: 'linux-x64-gnu', os: 'linux', cpu: 'x64', libc: 'glibc' },
  { target: 'aarch64-unknown-linux-gnu', abi: 'linux-arm64-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  { target: 'x86_64-unknown-linux-musl', abi: 'linux-x64-musl', os: 'linux', cpu: 'x64', libc: 'musl' },
  { target: 'aarch64-unknown-linux-musl', abi: 'linux-arm64-musl', os: 'linux', cpu: 'arm64', libc: 'musl' },
  { target: 'x86_64-pc-windows-msvc', abi: 'win32-x64-msvc', os: 'win32', cpu: 'x64' },
  { target: 'aarch64-pc-windows-msvc', abi: 'win32-arm64-msvc', os: 'win32', cpu: 'arm64' },
];

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const json = (path) => JSON.parse(readFileSync(path, 'utf8'));
const saveJson = (path, value) => writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);

export function stageRelease({ name, artifacts, out, target, registry = 'https://registry.npmjs.org/', root = ROOT }) {
  const registryUrl = new URL(registry);
  if (!['http:', 'https:'].includes(registryUrl.protocol) || registryUrl.username || registryUrl.password || registryUrl.search || registryUrl.hash) {
    throw new Error('--registry must be an HTTP(S) URL without credentials, query, or fragment');
  }
  registry = `${registryUrl.href.replace(/\/+$/, '')}/`;
  if (!/^(@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*$/.test(name ?? '')) {
    throw new Error('--name must be a lowercase npm package name (optionally scoped)');
  }
  const selected = target ? TARGETS.filter((item) => item.target === target || item.abi === target) : TARGETS;
  if (!selected.length) throw new Error(`Unsupported --target: ${target}`);
  if (existsSync(out)) throw new Error(`Output already exists: ${out}; use a new directory`);
  const cliSource = join(root, 'packages/fuck-npm');
  const bindingSource = join(root, 'packages/fuck-npm-binding');
  const cliManifest = json(join(cliSource, 'package.json'));
  const version = cliManifest.version;
  if (json(join(bindingSource, 'package.json')).version !== version) throw new Error('CLI and binding versions differ');
  for (const bin of ['fnpm', 'fnpm-lock', 'fnpmx', 'fnpx']) {
    if (!existsSync(join(cliSource, 'dist', `${bin}.js`))) throw new Error('Build the CLI bundle before staging');
  }
  checkBundle(cliSource);
  const bindingName = name.startsWith('@') ? `${name.split('/')[0]}/fnpm-binding` : `${name}-binding`;
  const prefix = `${bindingName}-`;
  const metadata = {
    version,
    license: 'MIT',
    repository: { type: 'git', url: 'git+https://github.com/SnowingFox/fuck-npm.git' },
    homepage: 'https://github.com/SnowingFox/fuck-npm#readme',
    bugs: 'https://github.com/SnowingFox/fuck-npm/issues',
    engines: cliManifest.engines,
    publishConfig: { access: 'public', registry },
  };
  // Validate the complete input set before writing anything. --target is only for smoke tests.
  const inputs = selected.map((item) => {
    const dir = existsSync(join(artifacts, `binding-${item.target}`))
      ? join(artifacts, `binding-${item.target}`) : join(artifacts, item.target);
    const suffix = item.os === 'win32' ? '.exe' : '';
    const files = [`fnpm.${item.abi}.node`, `fnpm${suffix}`, `fnpm-lock${suffix}`];
    for (const file of files) {
      if (!existsSync(join(dir, file))) throw new Error(`Missing release artifact: ${join(dir, file)}`);
    }
    return { ...item, dir, files };
  });
  mkdirSync(out, { recursive: true });
  const packages = [];
  function createPackage(dir, manifest) {
    mkdirSync(dir, { recursive: true });
    saveJson(join(dir, 'package.json'), manifest);
    cpSync(join(root, 'LICENSE'), join(dir, 'LICENSE'));
    cpSync(join(root, 'README.md'), join(dir, 'README.md'));
    packages.push({ name: manifest.name, version, directory: dir });
  }
  const optionalDependencies = Object.fromEntries(selected.map(({ abi }) => [`${prefix}${abi}`, version]));
  for (const item of inputs) {
    const dir = join(out, item.abi);
    createPackage(dir, {
      ...metadata, name: `${prefix}${item.abi}`, description: `fnpm native executables and addon for ${item.abi}`,
      main: `fnpm.${item.abi}.node`, files: item.files, os: [item.os], cpu: [item.cpu],
      ...(item.libc ? { libc: [item.libc] } : {}),
    });
    for (const file of item.files) {
      cpSync(join(item.dir, file), join(dir, file));
      if (!file.endsWith('.node')) chmodSync(join(dir, file), 0o755);
    }
  }
  const binding = join(out, 'binding');
  createPackage(binding, {
    ...metadata, name: bindingName, description: 'Native core for fnpm', main: 'index.js', types: 'index.d.ts',
    files: ['index.js', 'index.d.ts'], optionalDependencies,
  });
  cpSync(join(bindingSource, 'index.d.ts'), join(binding, 'index.d.ts'));
  const bindingLoader = readFileSync(join(bindingSource, 'index.js'), 'utf8')
    .replaceAll('@fuck-npm/binding', bindingName).replaceAll('Reinstall fuck-npm ', `Reinstall ${name} `);
  writeFileSync(join(binding, 'index.js'), bindingLoader);
  const cli = join(out, 'cli');
  createPackage(cli, {
    ...metadata, name, description: cliManifest.description, type: 'module', bin: cliManifest.bin,
    files: cliManifest.files, scripts: { postinstall: cliManifest.scripts.postinstall },
    dependencies: { [bindingName]: version }, optionalDependencies,
  });
  for (const entry of ['dist', 'bin', 'lib', 'scripts/postinstall.js']) {
    cpSync(join(cliSource, entry), join(cli, entry), { recursive: true });
  }
  // Keep source/workspace names unchanged; published loaders use the selected namespace.
  function rewrite(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) rewrite(path);
      else if (/\.(?:js|cjs)$/.test(entry.name)) {
        let text = readFileSync(path, 'utf8').replaceAll('@fuck-npm/binding', bindingName);
        // The bundle's manifest lookup must recognize the published CLI name too.
        text = text.replaceAll('"fuck-npm"', JSON.stringify(name)).replaceAll("'fuck-npm'", JSON.stringify(name));
        writeFileSync(path, text);
      }
    }
  }
  rewrite(cli);
  for (const bin of Object.values(cliManifest.bin)) chmodSync(join(cli, bin), 0o755);
  const release = { name, bindingName, version, registry, complete: !target, packages };
  saveJson(join(out, 'release.json'), release);
  return release;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const { values } = parseArgs({ options: {
      name: { type: 'string' }, artifacts: { type: 'string' }, out: { type: 'string' }, target: { type: 'string' }, registry: { type: 'string' },
    } });
    if (!values.artifacts || !values.out) throw new Error('Usage: node scripts/stage-release.mjs --name <npm-name> --artifacts <dir> --out <new-dir> [--registry <url>] [--target <target> (smoke only)]');
    console.log(JSON.stringify(stageRelease({ ...values, artifacts: resolve(values.artifacts), out: resolve(values.out) }), null, 2));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
