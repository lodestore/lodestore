import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { addCspMeta } from './csp-meta.mjs';

const hash = (text) => `'sha256-${createHash('sha256').update(text).digest('base64')}'`;
const META = /<meta http-equiv="content-security-policy" content="([^"]*)">/gi;
const dist = fileURLToPath(new URL('../dist', import.meta.url));

test('the meta follows the charset, precedes the first script and hashes both inline scripts', () => {
	const a = "document.documentElement.classList.add('js');";
	const b = 'window.x = 1;\n';
	const html = `<!doctype html><html><head><meta charset="utf-8"><script>${a}</script><script src="/x.js"></script><script type="module">${b}</script></head><body></body></html>`;
	const out = addCspMeta(html, 'page.html');
	const metas = [...out.matchAll(META)];
	assert.equal(metas.length, 1);
	const at = metas[0].index;
	assert.ok(out.indexOf('<meta charset="utf-8">') < at);
	assert.ok(at < out.indexOf('<script'));
	const policy = metas[0][1];
	assert.match(policy, /^script-src 'self' 'wasm-unsafe-eval' https:\/\/static\.cloudflareinsights\.com /);
	assert.deepEqual(policy.match(/'sha256-[^']+'/g), [hash(a), hash(b)]);
	assert.ok(!policy.includes("'unsafe-inline'"));
});

test('a script before the charset gets the meta directly in front of it', () => {
	const a = 'a();';
	const html = `<html><head><script>${a}</script><meta charset="utf-8"></head></html>`;
	const out = addCspMeta(html, 'page.html');
	assert.match(out, /<head><meta http-equiv="content-security-policy"[^>]*><script>a\(\);<\/script>/);
	assert.ok(out.includes(hash(a)));
});

test('a page without scripts still carries the meta', () => {
	const out = addCspMeta('<!doctype html><title>Moved</title><meta http-equiv="refresh" content="0;url=/x/">', 'r.html');
	assert.match(out, /^<!doctype html><meta http-equiv="content-security-policy"/);
	assert.equal([...out.matchAll(META)].length, 1);
});

test('an event-handler attribute or a javascript: link fails the build with the file name', () => {
	for (const body of ['<button onclick="go()">x</button>', '<img src="a.png" onload=go>', '<a href="javascript:go()">x</a>']) {
		assert.throws(() => addCspMeta(`<html><head><meta charset="utf-8"></head><body>${body}</body></html>`, 'dist/bad.html'), /dist\/bad\.html/);
	}
	const text = '<p>Use onclick= sparingly</p><script>const s = "<a onclick=1>";</script>';
	assert.doesNotThrow(() => addCspMeta(`<html><head><meta charset="utf-8"></head><body>${text}</body></html>`, 'ok.html'));
});

test('every built page has one meta before its first script that hashes all its inline scripts', () => {
	assert.ok(existsSync(join(dist, 'index.html')), 'docs/dist is missing, run pnpm --dir docs build first');
	const pages = readdirSync(dist, { recursive: true }).filter((f) => f.endsWith('.html'));
	assert.ok(pages.length > 0);
	for (const name of pages) {
		const html = readFileSync(join(dist, name), 'utf8');
		const metas = [...html.matchAll(META)];
		assert.equal(metas.length, 1, `${name}: expected one meta, found ${metas.length}`);
		const policy = metas[0][1];
		assert.ok(!policy.includes("'unsafe-inline'"), `${name}: 'unsafe-inline' in the meta`);
		const first = html.search(/<script[\s>]/i);
		if (first !== -1) {
			assert.ok(metas[0].index < first, `${name}: the meta is after the first script`);
			const charset = html.search(/<meta\s+charset=/i);
			if (charset !== -1 && charset < first) assert.ok(charset < metas[0].index, `${name}: the meta is before the charset`);
		}
		for (const m of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
			if (/\ssrc\s*=/i.test(m[1])) continue;
			assert.ok(policy.includes(hash(m[2])), `${name}: an inline script has no hash in the meta`);
		}
	}
});
