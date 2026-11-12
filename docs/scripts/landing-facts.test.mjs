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

function textOf(html) {
	return decode(
		html
			.replace(/<(script|style)\b[\s\S]*?<\/\1>/gi, '')
			.replace(/<\/(div|p|li|tr|h[1-6]|pre|caption)>|<br\s*\/?>/gi, '\n')
			.replace(/<[^>]+>/g, ''),
	);
}

function landing() {
	const file = here('../dist/index.html');
	assert.ok(existsSync(file), 'docs/dist/index.html is missing: run "pnpm --dir docs build" first');
	return readFileSync(file, 'utf8');
}

function source(name) {
	for (const ext of ['md', 'mdx']) {
		const file = here(`../src/content/docs/docs/${name}.${ext}`);
		if (existsSync(file)) return readFileSync(file, 'utf8');
	}
	throw new Error(`no source page for docs/${name} (.md or .mdx)`);
}

function element(html, tag, id) {
	const m = html.match(new RegExp(`<${tag}\\b[^>]*\\bid="${id}"[^>]*>([\\s\\S]*?)</${tag}>`));
	assert.ok(m, `<${tag} id="${id}"> not found in dist/index.html`);
	return m[1];
}

function fences(md) {
	return [...md.matchAll(/```[a-z]*\n([\s\S]*?)```/g)].map((m) => m[1].trim());
}

test('entity decoding is a single pass', () => {
	assert.equal(decode('&quot;a&quot; &amp;lt; &#39;b&#x27; &#47;'), `"a" &lt; 'b' /`);
});

test('Docker tab equals the quickstart server command', () => {
	const qs = source('quickstart');
	const version = qs.match(/^export LODESTORE_VERSION=(.+)$/m)?.[1];
	const host = qs.match(/^export LODESTORE_HOST=.+$/m)?.[0];
	const block = qs.match(/<!-- ci:run-server -->\n```sh\n([\s\S]*?)```/)?.[1];
	assert.ok(version && host && block, 'quickstart no longer has the version, the host or the ci:run-server block');
	const expected = `${host}\n${block.trim().replaceAll('$LODESTORE_VERSION', version)}`;
	assert.equal(textOf(element(landing(), 'pre', 'inst-docker')).trim(), expected);
});

test('Binary and Helm tabs are fenced blocks of the install pages', () => {
	const html = landing();
	for (const [id, page] of [
		['inst-binary', 'install/binary'],
		['inst-helm', 'install/helm'],
	]) {
		const shown = textOf(element(html, 'pre', id)).trim();
		assert.ok(shown.length > 0, `${id} is empty`);
		assert.ok(fences(source(page)).includes(shown), `${id} is not a fenced block of ${page}`);
	}
});

test('recovery terminal commands come from the restore recipe', () => {
	const recipe = source('recipes/restore-without-server');
	const lines = textOf(element(landing(), 'pre', 'tty'))
		.split('\n')
		.filter((l) => l.startsWith('$ '))
		.map((l) => l.slice(2));
	assert.ok(lines.length > 0, 'no command lines in the terminal');
	for (const line of lines) {
		const name = line.match(/^export ([A-Z_]+)=/)?.[1];
		if (name) assert.ok(recipe.includes(`export ${name}=`), `${name} is not exported in the recipe`);
		else assert.ok(recipe.includes(line), `not in the recipe: ${line}`);
	}
	assert.ok(lines.some((l) => l.startsWith('lode ')));
});

test('Policies as code YAML keys exist in the policies guide', () => {
	const guide = source('guides/policies');
	const yaml = textOf(landing().match(/<div class="pol-yaml"[^>]*>([\s\S]*?)<\/div>/)?.[1] ?? '');
	const rows = yaml.split('\n').filter((l) => /^\s*[A-Za-z]+:/.test(l));
	assert.ok(rows.length >= 6);
	for (const key of ['repository', 'paths']) assert.ok(rows.some((r) => r.trim().startsWith(`${key}:`)), `${key}: is missing from the sample`);
	for (const row of rows) {
		const [, key, value] = row.trim().match(/^([A-Za-z]+):\s*(.*)$/);
		assert.ok(guide.includes(`${key}:`), `${key}: is not in the policies guide`);
		if (value && !value.startsWith('[')) assert.ok(guide.includes(`${key}: ${value}`), `${key}: ${value} differs`);
	}
});

test('hero modal command is the HTTPS Add host command of the server', () => {
	const server = readFileSync(here('../../crates/lodestore-server/src/install/mod.rs'), 'utf8');
	const template = server.match(/format!\("(curl --proto[^"]*)"/)?.[1];
	assert.ok(template, 'install_command no longer has the https format! line in install/mod.rs');
	const expected = template.replace('{url}', 'https://lodestore.example.com').replace('{token}', '<join-token>');
	const text = textOf(landing());
	assert.ok(text.includes(expected), `no join command on the page: ${expected}`);
	assert.ok(!/\.internal\b/.test(text), 'an .internal host is on the landing page');
	assert.ok(!/lode1\.[A-Za-z0-9_-]{8,}/.test(text), 'a realistic join token is on the landing page');
	for (const m of text.matchAll(/--join (<join-token>|\S+)/g)) assert.equal(m[1], '<join-token>', `--join is followed by ${m[1]}`);
});

test('the hero heading is one sentence for crawlers', () => {
	const h1 = landing().match(/<h1\b[^>]*>[\s\S]*?<\/h1>/)?.[0];
	assert.ok(h1, 'no h1 on the landing page');
	assert.equal(textOf(h1).replace(/\s+/g, ' ').trim(), 'Back up every Linux host. Restore without the server.');
	assert.ok(!/database|docker|file share/i.test(h1), 'the heading promises something 0.x does not do');
	const raw = h1.match(/data-words="([^"]*)"/)?.[1];
	assert.ok(raw, 'no data-words on the rotator');
	const words = JSON.parse(decode(raw));
	assert.ok(Array.isArray(words) && words.length > 0);
	for (const w of words) assert.ok(typeof w === 'string' && w.length > 0 && w.endsWith('.'), `bad rotator word: ${w}`);
	const shown = h1.match(/<span>([^<]*)<\/span><\/span><\/span>/)?.[1];
	const longest = [shown, ...words].reduce((a, b) => (b.length > a.length ? b : a));
	assert.equal(decode(h1.match(/data-sizer="([^"]*)"/)?.[1] ?? ''), longest, 'the rotator does not reserve the width of its longest word');
});

test('trust line makes no air-gapped claim', () => {
	const text = textOf(landing());
	assert.ok(!/air-gapped/i.test(text), 'air-gapped is back');
	assert.ok(text.includes('Open source, control plane included'));
	assert.ok(text.includes('Self-hosted'));
});

const PRODUCTS = [
	['restic', 'restic'],
	['plakar', 'Plakar'],
	['bareos', 'Bareos'],
];

const compareMd = (id) => readFileSync(here(`../src/content/compare/${id}.md`), 'utf8');

test('comparison cells are quoted from the compare pages', () => {
	const section = element(landing(), 'section', 'compare');
	const table = section.match(/<table class="cmp"[\s\S]*?<\/table>/)?.[0];
	assert.ok(table, 'no comparison table');
	const mds = PRODUCTS.map(([id]) => compareMd(id));
	const rows = [...table.match(/<tbody>[\s\S]*<\/tbody>/)[0].matchAll(/<tr>([\s\S]*?)<\/tr>/g)];
	assert.ok(rows.length >= 4);
	for (const [, row] of rows) {
		const cells = [...row.matchAll(/<(?:th|td)\b[^>]*>([\s\S]*?)<\/(?:th|td)>/g)].map((m) => textOf(m[1]).trim());
		assert.equal(cells.length, 5, `row has ${cells.length} cells: ${cells[0]}`);
		assert.ok(mds.some((md) => md.includes(cells[1])), `Lodestore cell not in any compare page: ${cells[1]}`);
		PRODUCTS.forEach(([id], i) => assert.ok(mds[i].includes(cells[i + 2]), `${id} cell not in ${id}.md: ${cells[i + 2]}`));
	}
});

test('the comparison section has no checked-on line and no edition sentence', () => {
	const text = textOf(element(landing(), 'section', 'compare'));
	assert.ok(!/Checked against/i.test(text), 'the checked-on line is back');
	assert.ok(!/\d{4}-\d{2}-\d{2}/.test(text), 'a date is back in the comparison section');
	assert.ok(!/commercial Control Plane/i.test(text), 'the edition sentence is back');
	assert.ok(text.includes('Plakar (community)'));
});
