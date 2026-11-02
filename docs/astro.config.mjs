import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import starlightLinksValidator from 'starlight-links-validator';

const CI_MARKER = /<!--\s*ci:[a-z-]+\s*-->\n?/g;

const stripCiMarkers = {
	name: 'strip-ci-markers',
	hooks: {
		'astro:build:done': ({ dir }) => {
			const root = fileURLToPath(dir);
			for (const entry of readdirSync(root, { recursive: true })) {
				if (!entry.endsWith('.html')) continue;
				const file = join(root, entry);
				const html = readFileSync(file, 'utf8');
				const cleaned = html.replace(CI_MARKER, '');
				if (cleaned !== html) writeFileSync(file, cleaned);
			}
		},
	},
};

export default defineConfig({
	site: 'https://lodestore.github.io',
	base: '/lodestore',
	integrations: [
		starlight({
			title: 'Lodestore',
			plugins: [starlightLinksValidator()],
			editLink: {
				baseUrl: 'https://github.com/lodestore/lodestore/edit/main/docs/',
			},
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/lodestore/lodestore',
				},
			],
			customCss: ['./src/styles/custom.css'],
			sidebar: [
				{ label: 'Start', items: [{ slug: 'quickstart' }] },
				{ label: 'Install', items: [{ autogenerate: { directory: 'install' } }] },
				{ label: 'Guides', items: [{ autogenerate: { directory: 'guides' } }] },
				{ label: 'Recipes', items: [{ autogenerate: { directory: 'recipes' } }] },
				{ label: 'Reference', items: [{ autogenerate: { directory: 'reference' } }] },
				{ label: 'Security', items: [{ autogenerate: { directory: 'security' } }] },
				{ label: 'Compare', items: [{ autogenerate: { directory: 'compare' } }] },
				{ label: 'ADR', items: [{ autogenerate: { directory: 'adr' } }] },
			],
		}),
		stripCiMarkers,
	],
});
