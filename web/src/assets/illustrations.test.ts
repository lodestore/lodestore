import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const DIR = join(import.meta.dirname, "illustrations");
const MAX_BYTES = 61440;
const ILLUSTRATIONS = [
	"auth-hero",
	"hosts-empty",
	"repos-empty",
	"preview-overview",
	"preview-policies",
	"preview-jobs",
	"preview-restore",
	"preview-host-tabs",
];
const EXPECTED = ILLUSTRATIONS.flatMap((name) => [
	`${name}.light.webp`,
	`${name}.dark.webp`,
]);

describe("illustrations", () => {
	it.each(EXPECTED)("%s exists and weighs at most 60 KB", (file) => {
		expect(statSync(join(DIR, file)).size).toBeLessThanOrEqual(MAX_BYTES);
	});

	it("has no extra files", () => {
		expect(readdirSync(DIR).sort()).toEqual([...EXPECTED].sort());
	});
});
