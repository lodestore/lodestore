import { getCollection } from 'astro:content';

export const GITHUB_URL = 'https://github.com/lodestore/lodestore';
export const liveDemoUrl: string | undefined = import.meta.env.PUBLIC_LIVE_DEMO_URL || undefined;

export async function hasBlog(): Promise<boolean> {
	return (await getCollection('blog')).length > 0;
}

export async function navItems(): Promise<{ label: string; href: string }[]> {
	const items = [
		{ label: 'Product', href: '/' },
		{ label: 'Docs', href: '/docs/' },
	];
	if (await hasBlog()) items.push({ label: 'Blog', href: '/blog/' });
	if (liveDemoUrl) items.push({ label: 'Live demo', href: liveDemoUrl });
	return items;
}
