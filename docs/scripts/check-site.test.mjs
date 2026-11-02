import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./check-site.mjs', import.meta.url));

const page = (body = '', head = '') =>
	`<!doctype html><html><head><meta charset="utf-8"><script>localStorage.getItem('starlight-theme')</script>${head}</head><body>${body}</body></html>`;

const sitemap = (...paths) =>
	`<urlset>${paths.map((p) => `<url><loc>https://lodestore.dev${p}</loc></url>`).join('')}</urlset>`;

function site(overrides = {}) {
	const root = mkdtempSync(join(tmpdir(), 'check-site-'));
	const dist = join(root, 'dist');
	const files = {
		'index.html': page('<a href="/docs/">docs</a><img src="/pic.light.abc123.webp"><img src="/pic.dark.def456.webp">'),
		'404.html': page('<h1 id="top">Not found</h1>'),
		'docs/index.html': page('<a href="/">home</a>'),
		'pic.light.abc123.webp': 'light',
		'pic.dark.def456.webp': 'dark',
		'sitemap-index.xml': '<sitemapindex><sitemap><loc>https://lodestore.dev/sitemap-0.xml</loc></sitemap></sitemapindex>',
		'sitemap-0.xml': sitemap('/', '/docs/'),
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
	const r = run(site({ 'index.html': page('<a href="/missing/">x</a>') }));
	assert.equal(r.code, 1);
	assert.match(r.out, /site check failed/);
	assert.match(r.out, /\/missing\//);
});

test('an external script fails and an external link passes', () => {
	const bad = run(site({ 'index.html': page('', '<script src="https://cdn.example/x.js"></script>') }));
	assert.equal(bad.code, 1);
	assert.match(bad.out, /external resource/);
	const ok = run(site({ 'index.html': page('<a href="https://github.com/lodestore/lodestore">repo</a>') }));
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

test('an oversized file fails', () => {
	const r = run(site({ 'big.bin': 'x'.repeat(1.5 * 1024 * 1024 + 1) }));
	assert.equal(r.code, 1);
	assert.match(r.out, /big\.bin/);
});
