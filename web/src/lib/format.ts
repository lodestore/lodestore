import { copy } from "@/copy";

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

const number = new Intl.NumberFormat("en", { maximumFractionDigits: 1 });
const integer = new Intl.NumberFormat("en", { maximumFractionDigits: 0 });

export function formatBytes(bytes: number) {
	let value = bytes;
	let unit = 0;
	while (value >= 1024 && unit < UNITS.length - 1) {
		value /= 1024;
		unit += 1;
	}
	return `${number.format(value)} ${UNITS[unit]}`;
}

export const formatRate = (bytesPerSecond: number) =>
	`${formatBytes(bytesPerSecond)}/s`;

export const formatCount = (n: number) => integer.format(n);

export function formatDuration(ms: number) {
	const total = Math.max(0, Math.round(ms / 1000));
	const hours = Math.floor(total / 3600);
	const minutes = Math.floor((total % 3600) / 60);
	const seconds = total % 60;
	if (hours > 0) return `${hours} h ${minutes} min`;
	if (minutes > 0) return `${minutes} min ${seconds} s`;
	return `${seconds} s`;
}

export function formatTimeLeft(seconds?: number | null) {
	if (seconds == null || !Number.isFinite(seconds)) return copy.common.dash;
	if (seconds < 10) return copy.jobs.timeLeft.soon;
	if (seconds < 3600) {
		return copy.jobs.timeLeft.minutes(Math.max(1, Math.round(seconds / 60)));
	}
	const total = Math.round(seconds / 60);
	return copy.jobs.timeLeft.hoursMinutes(Math.floor(total / 60), total % 60);
}

const dateTime = new Intl.DateTimeFormat("en", {
	dateStyle: "medium",
	timeStyle: "short",
});
const clock = new Intl.DateTimeFormat("en-GB", {
	hour: "2-digit",
	minute: "2-digit",
	hourCycle: "h23",
});
const dayParts = new Intl.DateTimeFormat("en", {
	day: "numeric",
	month: "short",
	year: "numeric",
});

export function dayName(ms: number) {
	const part = (type: Intl.DateTimeFormatPartTypes) =>
		dayParts.formatToParts(ms).find((p) => p.type === type)?.value;
	return `${part("day")} ${part("month")} ${part("year")}`;
}

export const formatDateTime = (ms: number) => dateTime.format(ms);

const shortDateTime = new Intl.DateTimeFormat("en", {
	dateStyle: "short",
	timeStyle: "short",
});

/** `10/16/26, 11:36 AM`, for labels that share a narrow line. */
export const formatShortDateTime = (ms: number) => shortDateTime.format(ms);

const dateTimeSeconds = new Intl.DateTimeFormat("en", {
	dateStyle: "medium",
	timeStyle: "medium",
});

export const formatDateTimeSeconds = (ms: number) => dateTimeSeconds.format(ms);

export function formatMode(mode: number) {
	const bit = (mask: number, on: string) => (mode & mask ? on : "-");
	const special = (exec: number, flag: number, lower: string, upper: string) =>
		mode & flag ? (mode & exec ? lower : upper) : bit(exec, "x");
	return [
		bit(0o400, "r"),
		bit(0o200, "w"),
		special(0o100, 0o4000, "s", "S"),
		bit(0o040, "r"),
		bit(0o020, "w"),
		special(0o010, 0o2000, "s", "S"),
		bit(0o004, "r"),
		bit(0o002, "w"),
		special(0o001, 0o1000, "t", "T"),
	].join("");
}

export const formatClock = (ms: number) => clock.format(ms);

export function formatDay(ms: number, now = Date.now()) {
	const startOfDay = (value: number) => new Date(value).setHours(0, 0, 0, 0);
	const days = Math.round((startOfDay(now) - startOfDay(ms)) / 86_400_000);
	if (days === 0) return copy.restore.timeline.today;
	if (days === 1) return copy.restore.timeline.yesterday;
	return dayName(ms);
}

export function percent(progress: {
	bytes_done?: number;
	bytes_total?: number;
	files_done?: number;
	files_total?: number;
}) {
	const [done, total] =
		(progress.bytes_total ?? 0) > 0
			? [progress.bytes_done ?? 0, progress.bytes_total ?? 0]
			: [progress.files_done ?? 0, progress.files_total ?? 0];
	if (total <= 0) return 0;
	return Math.min(100, Math.max(0, Math.round((done / total) * 100)));
}

export function formatDurationShort(seconds: number) {
	for (const [unit, size] of [
		["d", 86_400],
		["h", 3_600],
		["m", 60],
	] as const) {
		if (seconds >= size && seconds % size === 0)
			return `${seconds / size}${unit}`;
	}
	return `${seconds}s`;
}

const slotFormats = new Map<string, Intl.DateTimeFormat>();

function slotFormat(timeZone: string) {
	let format = slotFormats.get(timeZone);
	if (!format) {
		const options: Intl.DateTimeFormatOptions = {
			weekday: "short",
			day: "numeric",
			month: "short",
			hour: "2-digit",
			minute: "2-digit",
			hourCycle: "h23",
		};
		try {
			format = new Intl.DateTimeFormat("en", { ...options, timeZone });
		} catch {
			format = new Intl.DateTimeFormat("en", { ...options, timeZone: "UTC" });
		}
		slotFormats.set(timeZone, format);
	}
	return format;
}

const partOf = (parts: Intl.DateTimeFormatPart[], type: string) =>
	parts.find((p) => p.type === type)?.value ?? "";

/** `Mon 20 Oct, 02:30` in the given zone. */
export function formatSlot(ms: number, timeZone: string) {
	const parts = slotFormat(timeZone).formatToParts(ms);
	const [weekday, day, month, hour, minute] = [
		"weekday",
		"day",
		"month",
		"hour",
		"minute",
	].map((type) => partOf(parts, type));
	return `${weekday} ${day} ${month}, ${hour}:${minute}`;
}

/** `Sun 25 Oct` for a calendar date such as `2026-10-25`. */
export function formatCalendarDay(isoDate: string) {
	const [year, month, day] = isoDate.split("-").map(Number);
	const parts = slotFormat("UTC").formatToParts(Date.UTC(year, month - 1, day));
	return `${partOf(parts, "weekday")} ${partOf(parts, "day")} ${partOf(parts, "month")}`;
}
