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

test('the secret file is private, named once and matches what quickstart-ci.sh writes', () => {
	assert.ok(qs.includes('lode-rk1-'), 'the page does not show the recovery key form');
	const block = qs.match(/<!-- ci:restore-without-server -->\n```sh\n([\s\S]*?)```/)?.[1];
	assert.ok(block, 'ci:restore-without-server block is missing');
	const file = block.match(/^export LODE_PASSWORD_FILE=(\S+)$/m)?.[1];
	assert.ok(file, 'the block does not set LODE_PASSWORD_FILE');
	const printfs = [...qs.matchAll(/^printf '%s' '[^']*' > (\S+)$/gm)];
	assert.ok(printfs.length >= 2, 'expected a printf for the password and one for the recovery key');
	for (const m of printfs) assert.equal(m[1], file, `printf writes ${m[1]}, LODE_PASSWORD_FILE is ${file}`);
	for (const b of shBlocks(qs).filter((b) => b.includes("printf '%s' '"))) {
		assert.ok(b.includes('umask 077') && b.indexOf('umask 077') < b.indexOf("printf '%s' '"), 'umask 077 must come before the printf of a secret');
	}
	const ci = read('../../scripts/quickstart-ci.sh');
	const written = ci.match(/printf '%s' "\$repo_password" >"\$workdir\/([^"]+)"/)?.[1];
	assert.equal(`./${written}`, file, 'LODE_PASSWORD_FILE differs from the file quickstart-ci.sh writes');
});

test('lode is installed only in one && chain after minisign -V', () => {
	const sec = joinLines(section(qs, '## Restore without the server'));
	assert.ok(sec.includes('releases/download') && sec.indexOf('releases/download') < sec.indexOf('minisign -V'), 'download must come before the signature check');
	const chains = sec.match(/minisign -V[^;&|\n]*&&\s*sudo install -m 0755 /g) ?? [];
	const installs = sec.match(/install -m 0755/g) ?? [];
	assert.ok(chains.length >= 1, 'no "minisign -V … && sudo install" chain');
	assert.equal(installs.length, chains.length, 'an install -m 0755 runs outside the minisign && chain');
	const need = section(qs, '## What you need');
	assert.ok(need.includes('minisign'), 'What you need does not name minisign');
	assert.ok(need.includes('/docs/install/binary/#agents-for-new-hosts'), 'What you need does not link the minisign package table');
});

test('the HTTPS section proxies REST only, trusts one network and joins without --insecure', () => {
	const sec = section(qs, '## With HTTPS');
	const sh = shBlocks(sec).join('\n');
	const join = joinLines(sh).split('\n').find((l) => l.includes('--join'));
	assert.ok(join, 'the HTTPS section has no --join command');
	assert.ok(join.includes("curl --proto '=https' --tlsv1.2"), 'the HTTPS join command does not force TLS 1.2 over https');
	assert.ok(!sh.includes('--insecure'), 'the HTTPS section uses --insecure');
	assert.ok(sh.includes('LODESTORE_PUBLIC_URL="https://'), 'LODESTORE_PUBLIC_URL is not https');
	assert.ok(sh.includes('LODESTORE_TRUSTED_PROXIES='), 'LODESTORE_TRUSTED_PROXIES is not set');
	assert.ok(sh.includes('reverse-proxy --from'), 'no Caddy reverse-proxy command');
	assert.ok(!sh.includes('-p 8080:8080'), 'the HTTPS section publishes port 8080');
});

test('every paragraph about --insecure outside the HTTPS section says anyone can get root', () => {
	const rest = qs.replace(section(qs, '## With HTTPS'), '');
	const paragraphs = rest.split(/\n\s*\n/).filter((p) => !p.includes('```') && p.includes('--insecure'));
	assert.ok(paragraphs.length >= 1, 'no paragraph warns about --insecure');
	for (const p of paragraphs) assert.match(p, /\broot\b/, `a paragraph about --insecure does not mention root: ${p.slice(0, 60)}`);
});

test('every minisign key on the quickstart and Binary pages is the project key', () => {
	const projectKey = read('../../crates/lodestore-agent/keys/project.pub').split('\n')[1].trim();
	for (const page of ['quickstart', 'install/binary']) {
		const md = read(`../src/content/docs/docs/${page}.md`);
		const keys = md.match(/RW[A-Za-z0-9+/]{40,}/g) ?? [];
		assert.ok(keys.length > 0, `${page} shows no minisign key`);
		for (const k of keys) assert.equal(k, projectKey, `${page} shows a key that is not project.pub`);
		for (const b of shBlocks(md)) {
			for (const l of joinLines(b).split('\n').filter((l) => /minisign -V/.test(l))) {
				if (/-p project\.pub\b/.test(l)) continue;
				assert.match(l, /-P RW[A-Za-z0-9+/]{40,}/, `${page}: minisign -V without a literal key: ${l.trim()}`);
			}
		}
		for (const m of md.matchAll(/minisign[^\n]*? -P (\S+)/g)) assert.match(m[1], /^RW[A-Za-z0-9+/]{40,}$/, `${page}: -P is not followed by a key`);
	}
});
