import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { limitKeywords } from './roadmap-limits.mjs';

const here = (path) => fileURLToPath(new URL(path, import.meta.url));
const read = (path) => readFileSync(here(path), 'utf8');

const qs = read('../src/content/docs/docs/quickstart.md');

function shBlocks(md) {
	return [...md.matchAll(/```sh\n([\s\S]*?)```/g)].map((m) => m[1]);
}

function section(md, heading) {
	const start = md.indexOf(`\n${heading}\n`);
	assert.ok(start >= 0, `no "${heading}" heading`);
	const rest = md.slice(start + heading.length + 2);
	const end = rest.search(/^## /m);
	return end < 0 ? rest : rest.slice(0, end);
}

const joinLines = (s) => s.replace(/\\\n\s*/g, ' ');

test('every variable in a sh block is assigned before its first use', () => {
	const ignored = new Set(['HOME', 'PATH', 'PWD']);
	const defined = new Set();
	shBlocks(qs).forEach((block, i) => {
		for (const line of block.split('\n')) {
			for (const m of line.matchAll(/(?:^|[\s;&|])(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=/g)) defined.add(m[1]);
			const loop = line.match(/^\s*for\s+([A-Za-z_][A-Za-z0-9_]*)\s+in\b/);
			if (loop) defined.add(loop[1]);
			for (const m of line.matchAll(/\$\{?([A-Za-z_][A-Za-z0-9_]*)/g)) {
				if (ignored.has(m[1])) continue;
				assert.ok(defined.has(m[1]), `$${m[1]} is used before it is set, in sh block ${i + 1}: ${line.trim()}`);
			}
		}
	});
});

test('docker run commands use distinct container names and data volumes', () => {
	const names = [];
	const volumes = [];
	for (const block of shBlocks(qs)) {
		for (const cmd of joinLines(block).split('\n').filter((l) => /\bdocker run\b/.test(l))) {
			for (const m of cmd.matchAll(/--name\s+(\S+)/g)) names.push(m[1]);
			for (const m of cmd.matchAll(/-v\s+([\w.-]+):\/var\/lib\/lodestore\b/g)) volumes.push(m[1]);
		}
	}
	assert.ok(names.length >= 2, 'expected several docker run commands');
	const dup = (list) => list.find((v, i) => list.indexOf(v) !== i);
	assert.equal(dup(names), undefined, `container name used twice: ${dup(names)}`);
	assert.equal(dup(volumes), undefined, `server data volume used twice: ${dup(volumes)}`);
});

test('CI markers are the four known ones, once each, and install-agent is one line', () => {
	const found = [...qs.matchAll(/<!-- ci:([a-z-]+) -->/g)].map((m) => m[1]);
	assert.deepEqual([...found].sort(), ['install-agent', 'restore-without-server', 'run-server', 'setup-link']);
	const block = qs.match(/<!-- ci:install-agent -->\n```sh\n([\s\S]*?)```/)?.[1];
	assert.ok(block, 'ci:install-agent block is missing');
	assert.equal(block.trim().split('\n').length, 1, 'ci:install-agent must stay one line');
});

test('the limits paragraph with the roadmap link comes before the install steps', () => {
	const link = qs.indexOf('](/roadmap/)');
	assert.ok(link >= 0, 'quickstart does not link /roadmap/');
	assert.ok(link < qs.indexOf('\n## Start the server'), 'the /roadmap/ link must come before "## Start the server"');
	const paragraph = qs.split(/\n\s*\n/).find((p) => p.includes('](/roadmap/)'));
	for (const re of limitKeywords) {
		assert.ok(re.test(paragraph), `the paragraph with the roadmap link does not mention ${re} (see roadmap-limits.mjs)`);
	}
});
