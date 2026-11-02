import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { pages } from './pages-redirects.mjs';

const script = fileURLToPath(new URL('./pages-redirects.mjs', import.meta.url));
const docsRoot = fileURLToPath(new URL('..', import.meta.url));
const dist = join(docsRoot, 'dist');

const countPages = (dir, pattern) =>
	readdirSync(join(docsRoot, 'src/content', dir), { recursive: true }).filter((f) => pattern.test(f)).length;

function generate() {
	const out = mkdtempSync(join(tmpdir(), 'pages-redirects-'));
	const r = spawnSync(process.execPath, [script, out], { encoding: 'utf8' });
	assert.equal(r.status, 0, `${r.stdout}${r.stderr}`);
	return out;
}

test('one stub per docs page and per comparison, plus 404.html', () => {
	const out = generate();
	const expected = countPages('docs/docs', /\.mdx?$/) + countPages('compare', /\.md$/);
	const stubs = readdirSync(out, { recursive: true }).filter((f) => f.endsWith('index.html'));
	assert.equal(stubs.length, expected);
	assert.ok(existsSync(join(out, '404.html')));
	assert.ok(existsSync(join(out, 'index.html')));
	assert.ok(existsSync(join(out, 'quickstart/index.html')));
	assert.ok(existsSync(join(out, 'compare/restic/index.html')));
});

test('every stub has canonical, meta refresh, hash-preserving script and a link to one target', () => {
	const out = generate();
	for (const file of readdirSync(out, { recursive: true }).filter((f) => f.endsWith('.html'))) {
		const html = readFileSync(join(out, file), 'utf8');
		assert.match(html, /rel="canonical"/, file);
		assert.match(html, /http-equiv="refresh"/, file);
		assert.match(html, /location\.hash/, file);
		const targets = new Set([
			...html.matchAll(/<link rel="canonical" href="([^"]+)"/g),
			...html.matchAll(/content="0; url=([^"]+)"/g),
			...html.matchAll(/location\.replace\("([^"]+)"/g),
			...html.matchAll(/<a href="([^"]+)"/g),
		].map((m) => m[1]));
		assert.equal(targets.size, 1, `${file}: ${[...targets]}`);
	}
});

test('every target is on lodestore.dev and exists in the built site', () => {
	assert.ok(existsSync(join(dist, 'index.html')), 'run `pnpm build` first');
	for (const { to } of pages()) {
		assert.ok(to.startsWith('https://lodestore.dev/'), to);
		const path = to.slice('https://lodestore.dev/'.length);
		assert.ok(existsSync(join(dist, path, 'index.html')), `${to} is not in dist`);
	}
});

test('known targets', () => {
	const target = (from) => pages().find((p) => p.from === from)?.to;
	assert.equal(target(''), 'https://lodestore.dev/docs/');
	assert.equal(target('quickstart'), 'https://lodestore.dev/docs/quickstart/');
	assert.equal(target('compare/restic'), 'https://lodestore.dev/compare/restic/');
});
