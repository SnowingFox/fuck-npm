import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';
import expressiveCode from 'astro-expressive-code';
import { join, dirname } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export default defineConfig({
  site: process.env.DOCS_SITE || undefined,
  base: process.env.DOCS_BASE || '/',
  trailingSlash: 'always',
  integrations: [
    expressiveCode({ themes: ['github-light', 'github-dark'] }),
    mdx(),
    ...(process.env.DOCS_SITE ? [sitemap()] : []),
    {
      name: 'fnpm-search',
      hooks: {
        'astro:build:done': ({ dir }) => {
          execFileSync(process.execPath, [join(dirname(fileURLToPath(import.meta.resolve('pagefind'))), 'runner/bin.cjs'), '--site', fileURLToPath(dir)], { stdio: 'inherit' });
        },
      },
    },
  ],
});
