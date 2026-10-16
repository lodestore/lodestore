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
