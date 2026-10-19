import { describe, expect, it } from "vitest";
import { formatRelative } from "@/components/relative-time";
import {
	formatBytes,
	formatCalendarDay,
	formatClock,
	formatCount,
	formatDateTime,
	formatDay,
	formatDuration,
	formatDurationShort,
	formatMode,
	formatRate,
	formatSlot,
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

describe("snapshot date formatters", () => {
	const at = (y: number, m: number, d: number, h = 0, min = 0) =>
		new Date(y, m, d, h, min).getTime();

	it("formats a date with a short time", () => {
		expect(formatDateTime(at(2026, 9, 14, 15, 4))).toMatch(
			/^Oct 14, 2026,\s+3:04\s+PM$/,
		);
	});

	it("formats the time of day as HH:mm", () => {
		expect(formatClock(at(2026, 9, 14, 7, 5))).toBe("07:05");
	});

	it("names today and yesterday and prints other days in full", () => {
		const now = at(2026, 9, 14, 0, 30);
		expect(formatDay(at(2026, 9, 14, 0, 1), now)).toBe("Today");
		expect(formatDay(at(2026, 9, 13, 23, 59), now)).toBe("Yesterday");
		expect(formatDay(at(2026, 9, 12, 12), now)).toBe("12 Oct 2026");
		expect(formatDay(at(2026, 8, 6, 12), now)).toBe("6 Sep 2026");
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

describe("formatMode", () => {
	it.each([
		[0o40755, "rwxr-xr-x"],
		[0o100644, "rw-r--r--"],
		[0o4755, "rwsr-xr-x"],
		[0o4644, "rwSr--r--"],
		[0o2755, "rwxr-sr-x"],
		[0o1777, "rwxrwxrwt"],
		[0o1776, "rwxrwxrwT"],
		[0, "---------"],
	])("%o -> %s", (mode, expected) => {
		expect(formatMode(mode)).toBe(expected);
	});
});

describe("schedule formatters", () => {
	it("prints durations in the largest whole unit", () => {
		expect(formatDurationShort(900)).toBe("15m");
		expect(formatDurationShort(86_400)).toBe("1d");
		expect(formatDurationShort(5_400)).toBe("90m");
		expect(formatDurationShort(7_200)).toBe("2h");
		expect(formatDurationShort(45)).toBe("45s");
		expect(formatDurationShort(0)).toBe("0s");
	});

	it("prints a slot in the zone of the policy", () => {
		const at = Date.UTC(2026, 9, 20, 0, 30);
		expect(formatSlot(at, "UTC")).toBe("Tue 20 Oct, 00:30");
		expect(formatSlot(at, "Europe/Kyiv")).toBe("Tue 20 Oct, 03:30");
		expect(formatSlot(at, "Australia/Lord_Howe")).toBe("Tue 20 Oct, 11:30");
		expect(formatSlot(at, "Not/AZone")).toBe("Tue 20 Oct, 00:30");
	});

	it("names a calendar day without shifting it by the local zone", () => {
		expect(formatCalendarDay("2026-10-25")).toBe("Sun 25 Oct");
		expect(formatCalendarDay("2027-03-01")).toBe("Mon 1 Mar");
	});
});

describe("formatRelative", () => {
	const now = new Date(Date.UTC(2026, 9, 17, 12));
	const later = (seconds: number) => new Date(now.getTime() + seconds * 1000);

	it("reads the past and the future", () => {
		expect(formatRelative(later(-7_200), now)).toBe("2 hours ago");
		expect(formatRelative(later(3 * 3_600), now)).toBe("in 3 hours");
		expect(formatRelative(later(2 * 86_400), now)).toBe("in 2 days");
	});

	it("keeps numbers when asked, so a label can follow it", () => {
		expect(formatRelative(later(86_400), now)).toBe("tomorrow");
		expect(formatRelative(later(86_400), now, "always")).toBe("in 1 day");
	});

	it("counts days by the calendar, not by 24-hour blocks", () => {
		const evening = new Date(2026, 9, 18, 20, 20);
		const at = (h: number) => new Date(evening.getTime() + h * 3_600_000);
		expect(formatRelative(at(31), evening)).toBe("in 2 days");
		expect(formatRelative(at(-26), evening)).toBe("yesterday");
	});

	it("says just now for the current second", () => {
		expect(formatRelative(now, now)).toBe("just now");
	});
});
