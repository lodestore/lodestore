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
	return formats[numeric].format(Math.round(seconds / size), unit);
}

export function RelativeTime({
	value,
	now = new Date(),
	numeric,
}: {
	value: string | Date;
	now?: Date;
	numeric?: "auto" | "always";
}) {
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
						{formatRelative(date, now, numeric)}
					</time>
				</TooltipTrigger>
				<TooltipContent>{absolute}</TooltipContent>
			</Tooltip>
		</TooltipProvider>
	);
}
