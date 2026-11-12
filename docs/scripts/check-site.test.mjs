import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./check-site.mjs', import.meta.url));

const icons = '<link rel="apple-touch-icon" href="/apple-touch-icon.png"><link rel="manifest" href="/site.webmanifest">';
const ld = '<script type="application/ld+json">{"@type":"SoftwareApplication"}</script>';

const page = (body = '', head = '') =>
	`<!doctype html><html><head><meta charset="utf-8"><script>localStorage.getItem('starlight-theme')</script>${icons}${head}</head><body>${body}</body></html>`;
const home = (body = '', head = '') => page(body, head + ld);

const sitemap = (...paths) =>
	`<urlset>${paths.map((p) => `<url><loc>https://lodestore.dev${p}</loc></url>`).join('')}</urlset>`;

function site(overrides = {}) {
	const root = mkdtempSync(join(tmpdir(), 'check-site-'));
	const dist = join(root, 'dist');
	const files = {
		'index.html': home('<a href="/docs/">docs</a><img src="/pic.light.abc123.webp" alt=""><img src="/pic.dark.def456.webp" alt="">'),
		'404.html': page('<h1 id="top">Not found</h1>'),
		'docs/index.html': page('<a href="/">home</a>'),
		'pic.light.abc123.webp': 'light',
		'pic.dark.def456.webp': 'dark',
		'sitemap-index.xml': '<sitemapindex><sitemap><loc>https://lodestore.dev/sitemap-0.xml</loc></sitemap></sitemapindex>',
		'sitemap-0.xml': sitemap('/', '/docs/'),
		'robots.txt': 'User-agent: *\nAllow: /\nSitemap: https://lodestore.dev/sitemap-index.xml\n',
		'favicon.ico': 'ico',
		'apple-touch-icon.png': 'png',
		'site.webmanifest': '{}',
		...overrides,
	};
	for (const [name, content] of Object.entries(files)) {
		if (content === null) continue;
		const file = join(dist, name);
		mkdirSync(dirname(file), { recursive: true });
		writeFileSync(file, content);
	}
	return dist;
}

function run(dist) {
	const r = spawnSync(process.execPath, [script, dist], { encoding: 'utf8' });
	return { code: r.status, out: `${r.stdout}${r.stderr}` };
}

test('a valid site passes', () => {
	const r = run(site());
	assert.equal(r.code, 0, r.out);
});

test('a link to a missing page fails', () => {
	const r = run(site({ 'index.html': home('<a href="/missing/">x</a>') }));
	assert.equal(r.code, 1);
	assert.match(r.out, /site check failed/);
	assert.match(r.out, /\/missing\//);
});

test('an external script fails and an external link passes', () => {
	const bad = run(site({ 'index.html': home('', '<script src="https://cdn.example/x.js"></script>') }));
	assert.equal(bad.code, 1);
	assert.match(bad.out, /external resource/);
	const ok = run(site({ 'index.html': home('<a href="https://example.org/repo">repo</a>') }));
	assert.equal(ok.code, 0, ok.out);
});

test('an anchor without a matching id fails', () => {
	const r = run(site({ 'docs/index.html': page('<a href="/404.html#nope">x</a>') }));
	assert.equal(r.code, 1);
	assert.match(r.out, /broken anchor/);
	const ok = run(site({ 'docs/index.html': page('<a href="/404.html#top">x</a>') }));
	assert.equal(ok.code, 0, ok.out);
});

test('a light image without a dark pair fails', () => {
	const r = run(site({ 'pic.dark.def456.webp': null }));
	assert.equal(r.code, 1);
	assert.match(r.out, /no dark pair/);
});

test('a page without starlight-theme fails', () => {
	const r = run(site({ 'docs/index.html': '<!doctype html><html><head></head><body></body></html>' }));
	assert.equal(r.code, 1);
	assert.match(r.out, /no starlight-theme/);
});

test('a sitemap without /docs/ fails', () => {
	const r = run(site({ 'sitemap-0.xml': sitemap('/') }));
	assert.equal(r.code, 1);
	assert.match(r.out, /no \/docs\//);
});

test('a site without robots.txt or with a robots.txt that lacks the sitemap fails', () => {
	const none = run(site({ 'robots.txt': null }));
	assert.equal(none.code, 1);
	assert.match(none.out, /robots\.txt/);
	const bare = run(site({ 'robots.txt': 'User-agent: *\nAllow: /\n' }));
	assert.equal(bare.code, 1);
	assert.match(bare.out, /Sitemap/);
});

test('missing icons, manifest links or home structured data fail', () => {
	const ico = run(site({ 'favicon.ico': null }));
	assert.equal(ico.code, 1);
	assert.match(ico.out, /favicon\.ico/);
	const bare = '<!doctype html><html><head><script>localStorage.getItem("starlight-theme")</script></head><body></body></html>';
	const links = run(site({ 'docs/index.html': bare }));
	assert.equal(links.code, 1);
	assert.match(links.out, /apple-touch-icon/);
	const noLd = run(site({ 'index.html': page() }));
	assert.equal(noLd.code, 1);
	assert.match(noLd.out, /ld\+json/);
	const wrong = run(site({ 'index.html': page('', '<script type="application/ld+json">{"@type":"Product"}</script>') }));
	assert.equal(wrong.code, 1);
	assert.match(wrong.out, /SoftwareApplication/);
});

test('an oversized file fails', () => {
	const r = run(site({ 'big.bin': 'x'.repeat(1.5 * 1024 * 1024 + 1) }));
	assert.equal(r.code, 1);
	assert.match(r.out, /big\.bin/);
});

const post = ({ type = 'article', image = 'https://lodestore.dev/_astro/post.abc.png', alt = 'A card', title = 'Post | Lodestore' } = {}) =>
	page(
		'<h1>Post</h1>',
		`<title>${title}</title><meta property="og:type" content="${type}">` +
			(image ? `<meta property="og:image" content="${image}">` : '') +
			(alt === null ? '' : `<meta property="og:image:alt" content="${alt}">`),
	);
const postSite = (opts, extra = {}) =>
	site({ 'blog/post/index.html': post(opts), '_astro/post.abc.png': 'png', ...extra });

test('an image without an alt attribute fails and an empty alt passes', () => {
	const bad = run(site({ 'docs/index.html': page('<img src="/pic.light.abc123.webp">') }));
	assert.equal(bad.code, 1);
	assert.match(bad.out, /img without alt/);
	const ok = run(site({ 'docs/index.html': page('<img src="/pic.light.abc123.webp" alt="">') }));
	assert.equal(ok.code, 0, ok.out);
});

test('a valid blog post passes', () => {
	const r = run(postSite());
	assert.equal(r.code, 0, r.out);
});

test('a blog post that is not og:type article fails', () => {
	const r = run(postSite({ type: 'website' }));
	assert.equal(r.code, 1);
	assert.match(r.out, /og:type is not article/);
});

test('a blog post without its own og:image fails', () => {
	const std = run(postSite({ image: 'https://lodestore.dev/og.png' }, { 'og.png': 'png' }));
	assert.equal(std.code, 1);
	assert.match(std.out, /has no own og:image/);
	const none = run(postSite({ image: '' }));
	assert.equal(none.code, 1);
	assert.match(none.out, /has no own og:image/);
});

test('an og:image that is not in dist fails', () => {
	const r = run(postSite({ image: 'https://lodestore.dev/_astro/gone.png' }));
	assert.equal(r.code, 1);
	assert.match(r.out, /og:image not in dist/);
});

test('a blog post without og:image:alt fails', () => {
	for (const alt of [null, '']) {
		const r = run(postSite({ alt }));
		assert.equal(r.code, 1);
		assert.match(r.out, /no og:image:alt/);
	}
});

test('a blog post title that repeats Lodestore fails', () => {
	for (const title of ['Lodestore news: Lodestore', 'News: Lodestore']) {
		const r = run(postSite({ title }));
		assert.equal(r.code, 1, title);
		assert.match(r.out, /title repeats Lodestore/);
	}
	const ok = run(postSite({ title: 'Introducing Lodestore 0.1' }));
	assert.equal(ok.code, 0, ok.out);
});
