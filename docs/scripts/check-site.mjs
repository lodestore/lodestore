import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { basename, dirname, join, relative, resolve, sep } from 'node:path';

const KB = 1024;
const MB = 1024 * KB;
const FIRST_SCREEN_BUDGET = 400 * KB;
const FILE_BUDGET = 1.5 * MB;
const GIF_BUDGET = 4 * MB;
const DIST_BUDGET = 25 * MB;

const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:)?\/\//i;
const SKIPPED_SCHEME = /^(?:mailto|data|tel|javascript):/i;
const ASSET_TAGS = new Set(['script', 'img', 'video', 'source', 'audio', 'iframe', 'track']);
const ASSET_RELS = new Set([
	'stylesheet',
	'preload',
	'modulepreload',
	'icon',
	'preconnect',
	'dns-prefetch',
	'manifest',
]);

const dist = resolve(process.argv[2] ?? 'dist');
const errors = [];
const fail = (file, reason) => errors.push(`site check failed: ${file}: ${reason}`);

function walk(dir) {
	const out = [];
	for (const entry of readdirSync(dir, { withFileTypes: true })) {
		const path = join(dir, entry.name);
		if (entry.isDirectory()) out.push(...walk(path));
		else out.push(path);
	}
	return out;
}

function parseTags(html) {
	const body = html
		.replace(/<!--[\s\S]*?-->/g, '')
		.replace(/<(script|style)\b([^>]*)>[\s\S]*?<\/\1>/gi, '<$1$2></$1>');
	const tags = [];
	for (const m of body.matchAll(/<([a-z][a-z0-9-]*)\b([^>]*)>/gi)) {
		const attrs = {};
		for (const a of m[2].matchAll(/([^\s=/"'>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
			attrs[a[1].toLowerCase()] = a[2] ?? a[3] ?? a[4] ?? '';
		}
		tags.push({ name: m[1].toLowerCase(), attrs });
	}
	return tags;
}

function urlsOf(tag) {
	const { name, attrs } = tag;
	const out = [];
	if (name === 'a' && attrs.href !== undefined) out.push(['href', attrs.href]);
	if (name === 'link' && attrs.href !== undefined) out.push(['href', attrs.href]);
	for (const key of ['src', 'poster']) {
		if (attrs[key] !== undefined) out.push([key, attrs[key]]);
	}
	if (attrs.srcset !== undefined) {
		for (const part of attrs.srcset.split(',')) {
			const url = part.trim().split(/\s+/)[0];
			if (url) out.push(['srcset', url]);
		}
	}
	return out;
}

function isExternalAsset(tag, attr) {
	if (tag.name === 'link') {
		if (attr !== 'href') return false;
		const rels = (tag.attrs.rel ?? '').toLowerCase().split(/\s+/);
		return rels.some((r) => ASSET_RELS.has(r));
	}
	return ASSET_TAGS.has(tag.name) && attr !== 'href';
}

function resolveTarget(pagePath, href) {
	const url = new URL(href, `http://site${pagePath}`);
	const path = decodeURIComponent(url.pathname);
	const candidates = path.endsWith('/')
		? [`${path}index.html`]
		: [path, `${path}/index.html`, `${path}.html`];
	for (const c of candidates) {
		const file = join(dist, c);
		if (existsSync(file) && statSync(file).isFile()) return { file, hash: url.hash.slice(1) };
	}
	return { file: null, hash: '' };
}

const files = walk(dist);
const rel = (f) => relative(dist, f).split(sep).join('/');
const pages = files.filter((f) => f.endsWith('.html'));
const idCache = new Map();
const idsOf = (file) => {
	if (!idCache.has(file)) {
		const html = readFileSync(file, 'utf8');
		idCache.set(file, new Set([...html.matchAll(/\sid=(?:"([^"]*)"|'([^']*)'|([^\s>]+))/g)].map((m) => m[1] ?? m[2] ?? m[3])));
	}
	return idCache.get(file);
};

for (const page of pages) {
	const html = readFileSync(page, 'utf8');
	const pagePath = `/${rel(page)}`;
	if (!html.includes('starlight-theme')) fail(rel(page), 'no starlight-theme');
	for (const tag of parseTags(html)) {
		for (const [attr, value] of urlsOf(tag)) {
			if (SKIPPED_SCHEME.test(value)) continue;
			if (EXTERNAL.test(value)) {
				if (isExternalAsset(tag, attr)) fail(rel(page), `external resource ${value}`);
				continue;
			}
			if (value === '') continue;
			const decoded = value.startsWith('#') ? { file: page, hash: value.slice(1) } : resolveTarget(pagePath, value);
			if (!decoded.file) {
				fail(rel(page), `broken link ${value}`);
				continue;
			}
			if (decoded.hash && decoded.file.endsWith('.html') && !idsOf(decoded.file).has(decodeURIComponent(decoded.hash))) {
				fail(rel(page), `broken anchor ${value}`);
			}
		}
	}
	for (const m of html.matchAll(/<style\b[^>]*>([\s\S]*?)<\/style>/gi)) cssExternal(rel(page), m[1]);
}

function cssExternal(name, css) {
	if (/url\(\s*['"]?(?:https?:)?\/\//i.test(css) || /@import\s+(?:url\(\s*)?['"]?(?:https?:)?\/\//i.test(css)) {
		fail(name, 'external resource in css');
	}
}
for (const f of files.filter((f) => f.endsWith('.css'))) cssExternal(rel(f), readFileSync(f, 'utf8'));

const pairs = new Map();
for (const f of files) {
	const m = /^(.+?)\.(light|dark)\.(.+)$/.exec(basename(f));
	if (!m) continue;
	const key = `${rel(dirname(f))}/${m[1]}|${m[3].split('.').pop()}`;
	pairs.set(key, { ...pairs.get(key), [m[2]]: rel(f) });
}
for (const pair of pairs.values()) {
	if (!pair.light) fail(pair.dark, 'dark image has no light pair');
	if (!pair.dark) fail(pair.light, 'light image has no dark pair');
}

let total = 0;
for (const f of files) {
	const size = statSync(f).size;
	total += size;
	const limit = f.endsWith('.gif') ? GIF_BUDGET : FILE_BUDGET;
	if (size > limit) fail(rel(f), `${size} bytes exceeds ${limit}`);
}
if (total > DIST_BUDGET) fail('dist', `${total} bytes exceeds ${DIST_BUDGET}`);

const indexFile = join(dist, 'index.html');
if (existsSync(indexFile)) {
	const html = readFileSync(indexFile, 'utf8');
	let first = statSync(indexFile).size;
	for (const tag of parseTags(html)) {
		const url = tag.name === 'script' ? tag.attrs.src : tag.name === 'link' ? tag.attrs.href : undefined;
		if (!url || EXTERNAL.test(url) || !/\.(css|js|mjs)(?:\?|$)/.test(url)) continue;
		const { file } = resolveTarget('/index.html', url);
		if (file) first += statSync(file).size;
	}
	if (first > FIRST_SCREEN_BUDGET) fail('index.html', `first screen ${first} bytes exceeds ${FIRST_SCREEN_BUDGET}`);
} else {
	fail('index.html', 'missing');
}

const blogDir = resolve(dist, '..', 'src', 'content', 'blog');
const posts = existsSync(blogDir) ? walk(blogDir).filter((f) => /\.mdx?$/.test(f)).length : 0;
if (posts > 0) {
	const rssFile = join(dist, 'blog', 'rss.xml');
	if (!existsSync(rssFile)) {
		fail('blog/rss.xml', 'missing');
	} else {
		const items = [...readFileSync(rssFile, 'utf8').matchAll(/<item>([\s\S]*?)<\/item>/g)];
		if (items.length !== posts) fail('blog/rss.xml', `${items.length} items, ${posts} posts`);
		for (const [, item] of items) {
			const link = /<link>([^<]*)<\/link>/.exec(item)?.[1] ?? '';
			if (!link.startsWith('https://')) fail('blog/rss.xml', `item link is not https: ${link}`);
		}
	}
}

const sitemapIndex = join(dist, 'sitemap-index.xml');
if (!existsSync(sitemapIndex)) {
	fail('sitemap-index.xml', 'missing');
} else {
	const paths = new Set();
	for (const [, loc] of readFileSync(sitemapIndex, 'utf8').matchAll(/<loc>([^<]*)<\/loc>/g)) {
		const file = join(dist, basename(new URL(loc).pathname));
		if (!existsSync(file)) {
			fail('sitemap-index.xml', `${loc} is not in dist`);
			continue;
		}
		for (const [, u] of readFileSync(file, 'utf8').matchAll(/<loc>([^<]*)<\/loc>/g)) paths.add(new URL(u).pathname);
	}
	for (const want of ['/', '/docs/']) {
		if (!paths.has(want)) fail('sitemap-index.xml', `no ${want} in sitemaps`);
	}
}

if (errors.length > 0) {
	for (const e of errors) console.error(e);
	process.exit(1);
}
console.log(`site check passed: ${pages.length} pages`);
