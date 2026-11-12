import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { limitKeywords, limitTitles } from './roadmap-limits.mjs';

const here = (path) => fileURLToPath(new URL(path, import.meta.url));

const ENTITIES = { quot: '"', apos: "'", amp: '&', lt: '<', gt: '>', nbsp: ' ' };

function decode(s) {
	return s.replace(/&(#x[0-9a-f]+|#[0-9]+|quot|apos|amp|lt|gt|nbsp);/gi, (_, e) => {
		if (e[0] !== '#') return ENTITIES[e.toLowerCase()];
		const hex = e[1].toLowerCase() === 'x';
		return String.fromCodePoint(Number.parseInt(e.slice(hex ? 2 : 1), hex ? 16 : 10));
	});
}

function textOf(html) {
	return decode(
		html
			.replace(/<(script|style)\b[\s\S]*?<\/\1>/gi, '')
			.replace(/<\/(div|p|li|tr|h[1-6]|pre|caption)>|<br\s*\/?>/gi, '\n')
			.replace(/<[^>]+>/g, ''),
	);
}

function page(path) {
	const file = here(`../dist/${path}`);
	assert.ok(existsSync(file), `docs/dist/${path} is missing: run "pnpm --dir docs build" first`);
	return readFileSync(file, 'utf8');
}

function section(html, id) {
	const m = html.match(new RegExp(`<h2\\b[^>]*\\bid="${id}"[^>]*>[\\s\\S]*?(?=<h2\\b|</article>)`));
	assert.ok(m, `<h2 id="${id}"> not found on /roadmap/`);
	return m[0];
}

function items(sectionHtml) {
	return [...sectionHtml.matchAll(/<li>([\s\S]*?)<\/li>/g)].map((m) => {
		const tag = m[1].match(/<span class="status-tag[^"]*">([\s\S]*?)<\/span>/);
		assert.ok(tag, `a limits item has no status tag: ${textOf(m[1]).trim()}`);
		return { text: textOf(m[1].replace(tag[0], '')).trim(), status: textOf(tag[1]).trim() };
	});
}

test('entity decoding is a single pass', () => {
	assert.equal(decode('&quot;a&quot; &amp;lt; &#39;b&#x27; &#47;'), `"a" &lt; 'b' /`);
});

test('roadmap has the Now, Next and Later sections', () => {
	const html = page('roadmap/index.html');
	for (const [id, name] of [
		['now', 'Now'],
		['next', 'Next'],
		['later', 'Later'],
	]) {
		const m = html.match(new RegExp(`<h2\\b[^>]*\\bid="${id}"[^>]*>([\\s\\S]*?)</h2>`));
		assert.ok(m && textOf(m[1]).includes(name), `h2 ${name} not found`);
	}
});

test('every Next and Later item carries its own status tag', () => {
	const html = page('roadmap/index.html');
	const next = items(section(html, 'next'));
	const later = items(section(html, 'later'));
	assert.ok(next.length + later.length >= 7, `expected at least 7 limits, found ${next.length + later.length}`);
	for (const i of next) assert.equal(i.status, 'Next', `"${i.text}" sits under Next`);
	for (const i of later) assert.equal(i.status, 'Later', `"${i.text}" sits under Later`);
});

test('every Not-in-0.x category of D-04 is on the roadmap', () => {
	const html = page('roadmap/index.html');
	const all = [...items(section(html, 'next')), ...items(section(html, 'later'))];
	for (const re of limitKeywords) {
		assert.ok(
			all.some((i) => re.test(i.text)),
			`no roadmap item matches ${re}`,
		);
	}
});

test('limits in roadmap.md and limitKeywords match both ways', () => {
	const titles = limitTitles();
	assert.ok(titles.length >= 7, `expected at least 7 limits in roadmap.md, found ${titles.length}`);
	for (const t of titles) {
		assert.ok(
			limitKeywords.some((re) => re.test(t)),
			`limit "${t}" matches no key in roadmap-limits.mjs`,
		);
	}
	for (const re of limitKeywords) {
		assert.ok(
			titles.some((t) => re.test(t)),
			`key ${re} matches no limit in roadmap.md`,
		);
	}
});

test('Next and Later promise no dates and no versions', () => {
	const html = page('roadmap/index.html');
	const from = html.indexOf('<h2 id="next"');
	assert.ok(from >= 0, '<h2 id="next"> not found');
	const text = textOf(html.slice(from, html.indexOf('</article>', from)));
	assert.doesNotMatch(text, /\b20\d\d\b/, 'a year in Next or Later');
	assert.doesNotMatch(text, /\bv?\d+\.\d+(\.\d+)?\b/, 'a version number in Next or Later');
});

test('single sign-on and other storage are Later', () => {
	const html = page('roadmap/index.html');
	const later = items(section(html, 'later'));
	for (const re of [/single sign-on|SSO/i, /SFTP|WebDAV/]) {
		assert.ok(
			later.some((i) => re.test(i.text)),
			`${re} is not under Later`,
		);
	}
});
