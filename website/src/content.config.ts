import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';
export const collections = {
  docs: defineCollection({
    loader: glob({ pattern: '**/*.{md,mdx}', base: './src/content/docs', generateId: ({ entry }) => entry.replace(/\.(md|mdx)$/, '') }),
    schema: z.object({ title: z.string(), description: z.string().optional(), template: z.string().optional() }),
  }),
};
