import { describe, expect, it } from "vitest";
import {
	formatBytes,
	formatCount,
	formatDuration,
	formatRate,
	formatTimeLeft,
	percent,
} from "@/lib/format";

describe("formatTimeLeft", () => {
	it.each([
		[null, "—"],
		[undefined, "—"],
		[5, "Less than 10 s"],
		[240, "About 4 min"],
		[3780, "About 1 h 3 min"],
		[7190, "About 2 h 0 min"],
	])("%s -> %s", (input, expected) => {
		expect(formatTimeLeft(input)).toBe(expected);
	});
});

describe("number formatters", () => {
	it("formats bytes, rates and counts", () => {
		expect(formatBytes(512)).toBe("512 B");
		expect(formatBytes(1536)).toBe("1.5 KiB");
		expect(formatRate(10 * 1024 ** 2)).toBe("10 MiB/s");
		expect(formatCount(12345)).toBe("12,345");
	});

	it("formats durations in the largest two units", () => {
		expect(formatDuration(900)).toBe("1 s");
		expect(formatDuration(65_000)).toBe("1 min 5 s");
		expect(formatDuration(3_780_000)).toBe("1 h 3 min");
	});
});

describe("percent", () => {
	it("prefers bytes, falls back to files and tolerates missing fields", () => {
		expect(percent({ bytes_done: 1, bytes_total: 4, files_done: 9 })).toBe(25);
		expect(percent({ files_done: 1, files_total: 2 })).toBe(50);
		expect(percent({})).toBe(0);
		expect(percent({ bytes_done: 9, bytes_total: 4 })).toBe(100);
	});
});
