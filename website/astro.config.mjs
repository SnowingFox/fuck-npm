import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import docs from './docs.json' with { type: 'json' };

// Reuse the adapted Bun docs.json navigation as the site's single sidebar source.
const groups = docs.navigation.tabs.flatMap((tab) => tab.groups);
export default defineConfig({
  // Set both for hosted builds; relative content links also work under a project subpath.
  site: process.env.DOCS_SITE || undefined,
  base: process.env.DOCS_BASE || '/',
  trailingSlash: 'always',
  integrations: [starlight({
    title: docs.name,
    description: 'fnpm documentation: a Rust-backed JavaScript package manager focused on install time and disk usage.',
    logo: { light: './public/logo/fnpm-light.svg', dark: './public/logo/fnpm-dark.svg', replacesTitle: true },
    favicon: docs.favicon,
    social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/SnowingFox/fuck-npm' }],
    editLink: { baseUrl: 'https://github.com/SnowingFox/fuck-npm/edit/main/website/' },
    customCss: ['./src/styles/custom.css'],
    sidebar: groups.map((group) => ({ label: group.group, items: group.pages.map((slug) => ({ slug: slug === 'index' ? '' : slug })) })),
  })],
});
