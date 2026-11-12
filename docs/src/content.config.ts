import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';

export const collections = {
	docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
	compare: defineCollection({
		loader: glob({ base: './src/content/compare', pattern: '*.md' }),
		schema: z.object({
			title: z.string(),
			description: z.string(),
		}),
	}),
	pages: defineCollection({
		loader: glob({ base: './src/content/pages', pattern: '*.md' }),
		schema: z.object({
			title: z.string(),
			description: z.string(),
			ready: z.array(z.string()).optional(),
			limits: z
				.array(z.object({ title: z.string(), status: z.enum(['Next', 'Later']), note: z.string().optional() }))
				.optional(),
		}),
	}),
	blog: defineCollection({
		loader: glob({ base: './src/content/blog', pattern: '**/*.{md,mdx}' }),
		schema: ({ image }) =>
			z.object({
				title: z.string(),
				description: z.string(),
				pubDate: z.coerce.date(),
				updatedDate: z.coerce.date().optional(),
				tags: z.array(z.string()).default([]),
				coverLight: image(),
				coverDark: image(),
				coverAlt: z.string(),
				ogImage: image(),
			}),
	}),
};
