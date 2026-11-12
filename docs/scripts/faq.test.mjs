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

const strip = (html) => decode(html.replace(/<[^>]+>/g, '')).replace(/\s+/g, ' ').trim();

function page(path) {
	const file = here(`../dist/${path}`);
	assert.ok(existsSync(file), `docs/dist/${path} is missing: run "pnpm --dir docs build" first`);
	return readFileSync(file, 'utf8');
}

const html = page('faq/index.html');
const article = html.match(/<article\b[\s\S]*?<\/article>/)?.[0];
assert.ok(article, '<article> not found on /faq/');
const h3 = [...article.matchAll(/<h3\b[^>]*>([\s\S]*?)<\/h3>/g)].map((m) => strip(m[1]));
const h2 = [...article.matchAll(/<h2\b[^>]*>([\s\S]*?)<\/h2>/g)].map((m) => strip(m[1]));

test('JSON-LD is one FAQPage that mirrors the h3 questions', () => {
	const blocks = [...html.matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)];
	assert.equal(blocks.length, 1, 'expected exactly one ld+json block on /faq/');
	assert.ok(!blocks[0][1].includes('<'), 'raw "<" in the JSON-LD block');
	const ld = JSON.parse(blocks[0][1]);
	assert.equal(ld['@type'], 'FAQPage');
	assert.ok(ld.mainEntity.length >= 4, 'fewer than four questions');
	assert.deepEqual(
		ld.mainEntity.map((q) => q.name),
		h3,
	);
	for (const q of ld.mainEntity) {
		assert.ok(q.acceptedAnswer.text.length > 20, `answer too short: ${q.name}`);
	}
});

test('the first group is present', () => {
	assert.ok(h2.includes('Data and restore'), `h2 found: ${h2.join(', ')}`);
});
