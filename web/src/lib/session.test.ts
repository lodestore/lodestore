import { describe, expect, it } from "vitest";
import { safeNext } from "./session";

describe("safeNext", () => {
	it.each(["/hosts", "/hosts?tab=jobs", "/%2F/x"])("keeps %s", (next) => {
		expect(safeNext(next)).toBe(next);
	});

	it.each([
		"//evil.example",
		"/\\evil.example",
		"/\t/evil.example",
		"/\n/evil.example",
		"/\r/evil.example",
		"https://evil.example",
		"hosts",
		undefined,
		42,
	])("drops %j", (next) => {
		expect(safeNext(next)).toBeUndefined();
	});
});
