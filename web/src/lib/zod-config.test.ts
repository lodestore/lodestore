import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { z } from "zod";
import "./zod-config";

describe("zod config", () => {
	it("turns off the eval-based fast path", () => {
		expect(z.config().jitless).toBe(true);
	});

	it("is the first import of the entry point", () => {
		const main = readFileSync("src/main.tsx", "utf8");
		expect(main.split("\n")[0]).toBe('import "./lib/zod-config";');
	});
});
