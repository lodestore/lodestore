import { useSyncExternalStore } from "react";
import {
	Tooltip,
	TooltipContent,
	TooltipProvider,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
	["year", 31_536_000],
	["month", 2_592_000],
	["day", 86_400],
	["hour", 3_600],
	["minute", 60],
	["second", 1],
];

const formats = {
	auto: new Intl.RelativeTimeFormat("en", { numeric: "auto" }),
	always: new Intl.RelativeTimeFormat("en", { numeric: "always" }),
};

export function formatRelative(
	date: Date,
	now: Date,
	numeric: "auto" | "always" = "auto",
): string {
	const seconds = Math.round((date.getTime() - now.getTime()) / 1000);
	const [unit, size] =
		UNITS.find(([, s]) => Math.abs(seconds) >= s) ?? UNITS[UNITS.length - 1];
	if (numeric === "auto" && Math.abs(seconds) < 5) return copy.common.justNow;
	const value =
		unit === "day" ? calendarDays(date, now) : Math.round(seconds / size);
	return formats[numeric].format(value, unit);
}

function calendarDays(date: Date, now: Date): number {
	const day = (d: Date) =>
		Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()) / 86_400_000;
	return day(date) - day(now);
}

const listeners = new Set<() => void>();
let tick = 0;
let timer: ReturnType<typeof setInterval> | undefined;

function subscribe(listener: () => void) {
	listeners.add(listener);
	timer ??= setInterval(() => {
		tick += 1;
		for (const notify of listeners) notify();
	}, 30_000);
	return () => {
		listeners.delete(listener);
		if (listeners.size === 0) {
			clearInterval(timer);
			timer = undefined;
		}
	};
}

/** Re-renders every 30 s so relative labels don't go stale. */
function useTick() {
	return useSyncExternalStore(
		subscribe,
		() => tick,
		() => tick,
	);
}

export function RelativeTime({
	value,
	now,
	numeric,
}: {
	value: string | Date;
	now?: Date;
	numeric?: "auto" | "always";
}) {
	useTick();
	const date = new Date(value);
	if (Number.isNaN(date.getTime())) return <span>{copy.common.dash}</span>;
	const absolute = date.toLocaleString("en", {
		dateStyle: "medium",
		timeStyle: "medium",
	});
	return (
		<TooltipProvider>
			<Tooltip>
				<TooltipTrigger asChild>
					<time dateTime={date.toISOString()} tabIndex={0}>
						{formatRelative(date, now ?? new Date(), numeric)}
					</time>
				</TooltipTrigger>
				<TooltipContent>{absolute}</TooltipContent>
			</Tooltip>
		</TooltipProvider>
	);
}
