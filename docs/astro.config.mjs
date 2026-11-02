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
	site: process.env.SITE_URL ?? 'https://lodestore.dev',
	build: { inlineStylesheets: 'never' },
	integrations: [
		starlight({
			title: 'Lodestore',
			disable404Route: true,
			logo: { src: './src/assets/mark.svg' },
			favicon: '/favicon.svg',
			plugins: [
				starlightLinksValidator({
					exclude: ({ link }) =>
						link === '/' || link.startsWith('/#') || link.startsWith('/blog') || link.startsWith('/compare'),
				}),
			],
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
			customCss: ['./src/styles/tokens.css', './src/styles/custom.css'],
			components: {
				SocialIcons: './src/components/starlight/SocialIcons.astro',
				ThemeProvider:'./src/components/starlight/ThemeProvider.astro',
				ThemeSelect: './src/components/starlight/ThemeSelect.astro',
			},
			sidebar: [
				{ label: 'Start', items: [{ slug: 'docs/quickstart' }] },
				{ label: 'Install', items: [{ autogenerate: { directory: 'docs/install' } }] },
				{ label: 'Guides', items: [{ autogenerate: { directory: 'docs/guides' } }] },
				{ label: 'Recipes', items: [{ autogenerate: { directory: 'docs/recipes' } }] },
				{ label: 'Reference', items: [{ autogenerate: { directory: 'docs/reference' } }] },
				{ label: 'Security', items: [{ autogenerate: { directory: 'docs/security' } }] },
				{ label: 'ADR', items: [{ autogenerate: { directory: 'docs/adr' } }] },
			],
		}),
		stripCiMarkers,
	],
});
