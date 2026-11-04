import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const EXISTING_META = /<meta http-equiv="content-security-policy"[^>]*>/gi;
const SCRIPT = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
const hasSrc = (attrs) => /(?:^|\s)src\s*=/i.test(attrs);

function assertNoHandlers(html, file) {
	const body = html
		.replace(/<!--[\s\S]*?-->/g, '')
		.replace(/<(script|style)\b([^>]*)>[\s\S]*?<\/\1>/gi, '<$1$2></$1>');
	for (const tag of body.matchAll(/<[a-z][a-z0-9-]*\b[^>]*>/gi)) {
		for (const a of tag[0].matchAll(/([^\s=/"'<>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
			const name = a[1].toLowerCase();
			const value = a[2] ?? a[3] ?? a[4] ?? '';
			if (/^on[a-z]+$/.test(name) || (['href', 'src'].includes(name) && /^\s*javascript:/i.test(value))) {
				throw new Error(`csp meta failed: ${file}: inline handler or javascript: URL (${name}) is blocked by the page policy`);
			}
		}
	}
}

export function addCspMeta(source, file) {
	const html = source.replace(EXISTING_META, '');
	assertNoHandlers(html, file);
	const hashes = new Set();
	for (const [, attrs, text] of html.matchAll(SCRIPT)) {
		if (!hasSrc(attrs)) hashes.add(`'sha256-${createHash('sha256').update(text).digest('base64')}'`);
	}
	const policy = ["script-src 'self' 'wasm-unsafe-eval' https://static.cloudflareinsights.com", ...hashes].join(' ');
	const meta = `<meta http-equiv="content-security-policy" content="${policy}">`;
	const script = html.search(/<script[\s>]/i);
	const charset = /<meta\s+charset=[^>]*>/i.exec(html);
	let at;
	if (charset && (script === -1 || charset.index < script)) at = charset.index + charset[0].length;
	else if (script !== -1) at = script;
	else {
		const m = /<head\b[^>]*>/i.exec(html) ?? /<!doctype[^>]*>/i.exec(html);
		at = m ? m.index + m[0].length : 0;
	}
	return html.slice(0, at) + meta + html.slice(at);
}

export function cspMeta() {
	return {
		name: 'csp-meta',
		hooks: {
			'astro:build:done': ({ dir }) => {
				const root = fileURLToPath(dir);
				for (const entry of readdirSync(root, { recursive: true })) {
					if (!entry.endsWith('.html')) continue;
					const file = join(root, entry);
					writeFileSync(file, addCspMeta(readFileSync(file, 'utf8'), file));
				}
			},
		},
	};
}
