import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const dist = fileURLToPath(new URL('../dist', import.meta.url));

function walk(dir, ext) {
	return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
		const p = join(dir, e.name);
		if (e.isDirectory()) return walk(p, ext);
		return p.endsWith(ext) ? [p] : [];
	});
}

test('every blog clip is a keyboard-operable toggle without autoplay or controls', () => {
	assert.ok(existsSync(dist), 'docs/dist is missing: run "pnpm --dir docs build" first');
	const tags = walk(dist, '.html').flatMap((f) => readFileSync(f, 'utf8').match(/<video\b[^>]*>/g) ?? []);
	assert.ok(tags.length > 0, 'no video in dist: the blog clips are gone?');
	for (const tag of tags) {
		assert.match(tag, /\stabindex="0"/, tag);
		assert.match(tag, /\srole="button"/, tag);
		assert.match(tag, /\saria-label="[^"]+"/, tag);
		assert.match(tag, /\sdata-label="[^"]+"/, tag);
		assert.doesNotMatch(tag, /\s(autoplay|controls)\b/, tag);
	}
});

test('the clip script handles Enter and Space on keydown', () => {
	const scripts = walk(join(dist, '_astro'), '.js').map((f) => readFileSync(f, 'utf8'));
	assert.ok(
		scripts.some((s) => s.includes('keydown') && s.includes('Enter')),
		'no built script handles keydown + Enter',
	);
});
