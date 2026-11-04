import { mkdirSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const content = fileURLToPath(new URL('../src/content/', import.meta.url));
const ORIGIN = 'https://lodestore.dev';

const names = (dir, pattern) =>
	readdirSync(join(content, dir), { recursive: true })
		.filter((f) => pattern.test(f))
		.map((f) => f.replace(pattern, '').split('\\').join('/'))
		.sort();

export function pages() {
	const docs = names('docs/docs', /\.mdx?$/).map((slug) =>
		slug === 'index' ? { from: '', to: `${ORIGIN}/docs/` } : { from: slug, to: `${ORIGIN}/docs/${slug}/` },
	);
	const compare = names('compare', /\.md$/).map((name) => ({
		from: `compare/${name}`,
		to: `${ORIGIN}/compare/${name}/`,
	}));
	return [...docs, ...compare];
}

const stub = (to) => `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Moved to lodestore.dev</title>
<link rel="canonical" href="${to}">
<meta http-equiv="refresh" content="0; url=${to}">
<script>location.replace("${to}" + location.hash)</script>
</head>
<body>
<a href="${to}">Moved to lodestore.dev</a>
</body>
</html>
`;

function write(out, file, to) {
	const path = join(out, file);
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, stub(to));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const out = process.argv[2];
	if (!out) {
		console.error('usage: node pages-redirects.mjs <out>');
		process.exit(2);
	}
	for (const { from, to } of pages()) write(out, join(from, 'index.html'), to);
	write(out, '404.html', `${ORIGIN}/docs/`);
}
