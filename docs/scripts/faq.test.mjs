import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { limitKeywords } from './roadmap-limits.mjs';

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

test('the four groups come in order and there are at least twelve questions', () => {
	assert.deepEqual(h2, ['Data and restore', 'Security', 'Project and license', 'Operations']);
	assert.ok(h3.length >= 12, `only ${h3.length} questions`);
});

test('the page names no price, plan or edition', () => {
	const text = strip(article);
	assert.ok(!/\b(price|pricing|paid|subscription|edition|commercial)\b/i.test(text), 'D-11: money words on /faq/');
});

test('the answer about what is missing matches every limit of the roadmap', () => {
	assert.ok(article.includes('href="/roadmap/"'), 'no link to /roadmap/ in the article');
	const md = readFileSync(here('../src/content/pages/faq.md'), 'utf8');
	const block = md.split(/^(?=#{2,3} )/m).find((b) => b.startsWith('### ') && b.includes('](/roadmap/)'));
	assert.ok(block, 'no ### section links to /roadmap/');
	for (const re of limitKeywords) {
		assert.ok(re.test(block), `the roadmap answer does not mention ${re}`);
	}
});

test('FAQ is linked from the header and the footer', () => {
	for (const file of ['index.html', 'faq/index.html']) {
		const doc = page(file);
		for (const [where, re] of [
			['nav#site-nav', /<nav\b[^>]*id="site-nav"[\s\S]*?<\/nav>/],
			['footer.site-footer', /<footer\b[^>]*class="site-footer"[\s\S]*?<\/footer>/],
		]) {
			assert.ok(doc.match(re)?.[0].includes('href="/faq/"'), `${file}: no /faq/ link in ${where}`);
		}
	}
});
