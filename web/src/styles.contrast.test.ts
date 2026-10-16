/// <reference types="node" />
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync("src/styles.css", "utf8");

function tokens(selector: RegExp): Record<string, string> {
	const block = selector.exec(css)?.[1];
	if (!block) throw new Error(`block ${selector} not found in styles.css`);
	return Object.fromEntries(
		[...block.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)].map((m) => [
			m[1],
			m[2],
		]),
	);
}

function luminance(hex: string) {
	const [r, g, b] = [1, 3, 5].map((i) => {
		const c = Number.parseInt(hex.slice(i, i + 2), 16) / 255;
		return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
	});
	return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string) {
	const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
	return (hi + 0.05) / (lo + 0.05);
}

const text: [string, string][] = [
	["foreground", "background"],
	["foreground", "card"],
	["popover-foreground", "popover"],
	["muted-foreground", "background"],
	["muted-foreground", "card"],
	["muted-foreground", "sidebar"],
	["muted-foreground", "muted"],
	["primary-foreground", "primary"],
	["primary-strong", "background"],
	["primary-strong", "card"],
	["primary-strong", "sidebar"],
	["primary-strong", "muted"],
	["foreground", "primary-subtle"],
	["primary-strong", "primary-subtle"],
	["muted-foreground", "primary-subtle"],
	["destructive", "background"],
	["destructive", "card"],
	["destructive", "muted"],
	["success", "background"],
	["success", "card"],
	["success", "muted"],
	["warning", "background"],
	["warning", "card"],
	["warning", "muted"],
	["neutral", "background"],
	["neutral", "card"],
	["neutral", "muted"],
];

const graphics: [string, string][] = [
	["input", "background"],
	["input", "card"],
	["brand", "background"],
	["ring", "background"],
	["ring", "card"],
	["primary-strong", "muted"],
	["primary-strong", "background"],
];

const themes = {
	light: tokens(/(?:^|\n):root\s*\{([^}]*)\}/),
	dark: tokens(/\n\.dark\s*\{([^}]*)\}/),
};

function check(
	theme: string,
	t: Record<string, string>,
	pairs: [string, string][],
	min: number,
) {
	for (const [fg, bg] of pairs) {
		expect(t[fg], `${theme}: --${fg} missing`).toBeDefined();
		expect(t[bg], `${theme}: --${bg} missing`).toBeDefined();
		const ratio = contrast(t[fg], t[bg]);
		expect(
			ratio,
			`${theme}: --${fg} on --${bg} is ${ratio.toFixed(2)}:1, needs ${min}:1`,
		).toBeGreaterThanOrEqual(min);
	}
}

describe.each(Object.entries(themes))("%s theme contrast", (theme, t) => {
	it("text pairs reach 4.5:1", () => check(theme, t, text, 4.5));
	it("borders and graphics reach 3:1", () => check(theme, t, graphics, 3));
});
