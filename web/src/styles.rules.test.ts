/// <reference types="node" />
import { readdirSync, readFileSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { describe, expect, it } from "vitest";

const SRC = join(process.cwd(), "src");

function tsxFiles(dir: string): string[] {
	return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
		const p = join(dir, e.name);
		if (e.isDirectory()) return e.name === "gen" ? [] : tsxFiles(p);
		return p.endsWith(".tsx") ? [p] : [];
	});
}

const files = tsxFiles(SRC).map((p) => {
	const text = readFileSync(p, "utf8");
	return {
		rel: relative(SRC, p).split(sep).join("/"),
		text,
		lines: text.split("\n"),
	};
});

function violations(rule: string, re: RegExp, skipUi: boolean): string[] {
	return files
		.filter((f) => !(skipUi && f.rel.startsWith("components/ui/")))
		.flatMap((f) =>
			f.lines.flatMap((line, i) =>
				re.test(line) ? [`${f.rel}:${i + 1} violates "${rule}"`] : [],
			),
		);
}

describe("style rules", () => {
	const everywhere: [string, RegExp][] = [
		["no font weight 500", /\bfont-(medium|\[500\])(?![\w-])/],
		["no arbitrary text sizes", /\btext-\[\d+(\.\d+)?px\]/],
		["no text-4xl to text-9xl", /\btext-[4-9]xl\b/],
		["no six-digit hex literals", /#[0-9a-fA-F]{6}\b/],
		[
			"accent text and borders use primary-strong",
			/(?<![\w-])(?:text|border)-primary(?![\w-])/,
		],
	];
	for (const [rule, re] of everywhere) {
		it(rule, () => {
			expect(violations(rule, re, false)).toEqual([]);
		});
	}

	const offScale: [string, RegExp][] = [
		["no gap-3 or gap-5", /(?<![\w-])gap-[35](?![\w.-])/],
		["no gap-x-3 or gap-y-3", /(?<![\w-])gap-[xy]-3(?![\w.-])/],
		["no px-3 or py-3", /(?<![\w-])p[xy]-3(?![\w.-])/],
		[
			"no half-step gap, padding or margin",
			/(?<![\w-])(?:gap(?:-[xy])?|[pm][xylrtb]?)-\d+\.5(?![\w-])/,
		],
		["no pr-28", /(?<![\w-])pr-28(?![\w-])/],
	];
	for (const [rule, re] of offScale) {
		it(`${rule} outside components/ui`, () => {
			expect(violations(rule, re, true)).toEqual([]);
		});
	}

	const literals: [string, RegExp][] = [
		["no text literals in JSX", /(?<![=-])(?<!\S )>\s*[A-Za-z][^<>{}]*[<{]/g],
		[
			"no string literals in aria-label, placeholder or title",
			/\b(?:aria-label|placeholder|title)=(?:"[^"]*[A-Za-z]|\{\s*["'`])/g,
		],
	];
	for (const [rule, re] of literals) {
		it(`${rule} outside components/ui (copy lives in copy.ts)`, () => {
			const found = files
				.filter(
					(f) =>
						!f.rel.startsWith("components/ui/") &&
						!f.rel.startsWith("test/") &&
						!f.rel.includes(".test."),
				)
				.flatMap((f) =>
					[...f.text.matchAll(re)].map((m) => {
						const line = f.text.slice(0, m.index).split("\n").length;
						return `${f.rel}:${line} violates "${rule}"`;
					}),
				);
			expect(found).toEqual([]);
		});
	}
});
