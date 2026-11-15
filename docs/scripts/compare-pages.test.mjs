import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const dir = fileURLToPath(new URL('../src/content/compare/', import.meta.url));
const pages = readdirSync(dir)
	.filter((f) => f.endsWith('.md'))
	.map((f) => [f, readFileSync(`${dir}${f}`, 'utf8')]);

const WITH_STORAGE = new Set(['proxmox-backup-server.md', 'kopia.md', 'backrest.md']);

function section(text, heading) {
	const start = text.indexOf(`\n${heading}\n`);
	assert.ok(start >= 0, `missing ${heading}`);
	const rest = text.slice(start + heading.length + 2);
	const end = rest.search(/\n## /);
	return end < 0 ? rest : rest.slice(0, end);
}

for (const [file, text] of pages) {
	test(`${file}: structure`, () => {
		const lines = text.split('\n');
		assert.equal(lines[0], '---');
		assert.match(lines[1], /^title: Lodestore vs \S/);
		assert.ok(text.includes('\n## Why teams pick Lodestore\n'), 'no "Why teams pick Lodestore"');
		assert.ok(text.includes('\n## At a glance\n'), 'no "At a glance"');
		const header = text.split('\n').find((l) => /^\|\s*\|/.test(l));
		assert.ok(header?.includes('Lodestore 0.2'), 'the table header lacks "Lodestore 0.2"');
		if (WITH_STORAGE.has(file)) assert.ok(text.includes('\n### Storage\n'), 'no "### Storage" table');
	});

	test(`${file}: no dates or source tables`, () => {
		const body = text.slice(text.indexOf('\n---\n', 4) + 5);
		assert.doesNotMatch(body, /\d{4}-\d{2}-\d{2}/);
		assert.doesNotMatch(body, /Checked on/);
	});

	test(`${file}: sources and as-of date`, () => {
		const front = text.slice(0, text.indexOf('\n---\n', 4));
		assert.match(front, /^asOf: \d{4}-\d{2}-\d{2}$/m);
		const block = front.split(/^sources:\n/m)[1];
		assert.ok(block, 'no sources block');
		const items = block.split(/^ {2}- /m).slice(1);
		assert.ok(items.length > 0, 'sources is empty');
		const urls = items.map((item) => {
			const field = (name) => item.match(new RegExp(`(?:^|\\n) {0,4}${name}: "([^"]+)"`))?.[1];
			assert.ok(field('title'), `a source has no title: ${item}`);
			assert.ok(field('version'), `a source has no version: ${item}`);
			assert.match(field('url') ?? '', /^https:\/\/\S+$/, `a source has no https url: ${item}`);
			return field('url');
		});
		assert.equal(new Set(urls).size, urls.length, 'a source url is listed twice');
	});
}

test('bareos.md: the delete point is not sold as a difference', () => {
	const text = readFileSync(`${dir}bareos.md`, 'utf8');
	assert.doesNotMatch(
		section(text, '## Why teams pick Lodestore'),
		/cannot delete|can't delete/i,
		'Bareos clients hold no object-store keys either: it is not a difference',
	);
});

test('proxmox-backup-server.md: scenarios are split honestly', () => {
	const text = readFileSync(`${dir}proxmox-backup-server.md`, 'utf8');
	const row = text.split('\n').find((l) => l.startsWith('| Proxmox VM'));
	assert.ok(row, 'no "Proxmox VM" row');
	const lastCell = row.split('|').filter((c) => c.trim()).pop();
	assert.match(lastCell, /Not yet/);
	assert.ok(lastCell.includes('](/roadmap/)'), 'the Lodestore cell does not link to /roadmap/');
	assert.doesNotMatch(
		section(text, '## Why teams pick Lodestore'),
		/cannot delete|can't delete|without (the|a) (running )?server|write-only/i,
		'Proxmox Backup Server has these too: they are not a difference',
	);
});
