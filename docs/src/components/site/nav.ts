import { getCollection } from 'astro:content';

export const GITHUB_URL = 'https://github.com/lodestore/lodestore';
export const liveDemoUrl: string | undefined = import.meta.env.PUBLIC_LIVE_DEMO_URL || undefined;

let blog: Promise<boolean> | undefined;

export function hasBlog(): Promise<boolean> {
	blog ??= getCollection('blog').then((posts) => posts.length > 0);
	return blog;
}

export async function navItems(): Promise<{ label: string; href: string }[]> {
	const items = [
		{ label: 'Product', href: '/#feat' },
		{ label: 'Docs', href: '/docs/' },
		{ label: 'Compare', href: '/#compare' },
	];
	if (await hasBlog()) items.push({ label: 'Blog', href: '/blog/' });
	if (liveDemoUrl) items.push({ label: 'Live demo', href: liveDemoUrl });
	return items;
}
