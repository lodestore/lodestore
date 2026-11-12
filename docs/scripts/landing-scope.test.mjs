import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const here = (path) => fileURLToPath(new URL(path, import.meta.url));

const ENTITIES = { quot: '"', apos: "'", amp: '&', lt: '<', gt: '>', nbsp: ' ' };

function decode(s) {
	return s.replace(/&(#x[0-9a-f]+|#[0-9]+|quot|apos|amp|lt|gt|nbsp);/gi, (_, e) => {
		if (e[0] !== '#') return ENTITIES[e.toLowerCase()];
		const hex = e[1].toLowerCase() === 'x';
		return String.fromCodePoint(Number.parseInt(e.slice(hex ? 2 : 1), hex ? 16 : 10));
	});
}

const textOf = (html) => decode(html.replace(/<[^>]+>/g, '')).replace(/\s+/g, ' ').trim();

function dist(path) {
	const file = here(`../dist/${path}`);
	assert.ok(existsSync(file), `docs/dist/${path} is missing: run "pnpm --dir docs build" first`);
	return readFileSync(file, 'utf8');
}

function section(html, id) {
	const m = html.match(new RegExp(`<section\\b[^>]*\\bid="${id}"[^>]*>[\\s\\S]*?</section>`));
	assert.ok(m, `<section id="${id}"> not found in dist/index.html`);
	return m[0];
}

function pairs(html) {
	return [...html.matchAll(/<li>\s*<span>([\s\S]*?)<\/span>\s*<span class="status-tag[^"]*">([\s\S]*?)<\/span>/g)].map(
		(m) => `${textOf(m[1])} | ${textOf(m[2])}`,
	);
}

test('the scope section sits between install and compare', () => {
	const html = dist('index.html');
	const at = ['install', 'scope', 'compare'].map((id) => html.indexOf(`id="${id}"`));
	assert.ok(at.every((i) => i >= 0), 'install, scope and compare must all be on the landing');
	assert.ok(at[0] < at[1] && at[1] < at[2], 'the order must be install, scope, compare');
});

test('the scope heading and the roadmap link', () => {
	const scope = section(dist('index.html'), 'scope');
	assert.equal(textOf(scope.match(/<h2\b[\s\S]*?<\/h2>/)[0]), 'What 0.2 covers and what comes next.');
	assert.ok(scope.includes('href="/roadmap/"'), 'no link to /roadmap/');
});

test('the scope limits equal the limits of /roadmap/', () => {
	const scope = pairs(section(dist('index.html'), 'scope'));
	const limits = dist('roadmap/index.html').match(/<ul class="limits">[\s\S]*?<\/ul>/g) ?? [];
	const roadmap = limits.flatMap(pairs);
	assert.ok(scope.length >= 7, `expected at least 7 limits on the landing, found ${scope.length}`);
	assert.deepEqual([...scope].sort(), [...roadmap].sort());
});

test('Ready today lists at least four items', () => {
	const ready = section(dist('index.html'), 'scope').match(/<li class="ok">/g) ?? [];
	assert.ok(ready.length >= 4, `expected at least 4 ready items, found ${ready.length}`);
});

test('the install section no longer carries the platform line', () => {
	assert.ok(!/class="plat"/.test(dist('index.html')));
});

test('Compare in detail: six chips to the compare pages', () => {
	const compare = section(dist('index.html'), 'compare');
	const chips = compare.match(/<div class="cmp-chips[^"]*">[\s\S]*?<\/div>/)?.[0];
	assert.ok(chips, 'no .cmp-chips under the comparison table');
	assert.equal(textOf(chips.match(/<p class="lbl">[\s\S]*?<\/p>/)[0]), 'Compare in detail');
	const links = [...chips.matchAll(/<a href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/g)].map((m) => [m[1], textOf(m[2])]);
	assert.deepEqual(links, [
		['/compare/restic/', 'vs restic'],
		['/compare/plakar/', 'vs Plakar'],
		['/compare/bareos/', 'vs Bareos'],
		['/compare/proxmox-backup-server/', 'vs Proxmox Backup Server'],
		['/compare/kopia/', 'vs Kopia'],
		['/compare/backrest/', 'vs Backrest'],
	]);
	for (const [href] of links) dist(`${href.slice(1)}index.html`);
});
