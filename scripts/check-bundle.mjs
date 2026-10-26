import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

/** Validate tsup's generated literal imports, including lazy command chunks. */
export function checkBundle(directory, packedFiles) {
  const root = resolve(directory);
  const included = packedFiles && new Set(packedFiles);
  function visit(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = join(dir, entry.name);
      if (entry.isDirectory()) { visit(file); continue; }
      if (!entry.name.endsWith('.js')) continue;
      const source = readFileSync(file, 'utf8');
      const imports = /\b(?:from\s*|import\s*\(\s*|import\s*)["'](\.[^"']+)["']/g;
      for (const match of source.matchAll(imports)) {
        const dependency = fileURLToPath(new URL(match[1], pathToFileURL(file)));
        const path = relative(root, dependency).split('\\').join('/');
        if (path.startsWith('../') || !existsSync(dependency) || (included && !included.has(path))) {
          throw new Error(`Incomplete CLI bundle: ${relative(root, file)} imports missing ${path}`);
        }
      }
    }
  }
  visit(join(root, 'dist'));
}
