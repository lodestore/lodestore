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

const rtf = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

export function formatRelative(date: Date, now: Date): string {
	const seconds = Math.round((date.getTime() - now.getTime()) / 1000);
	const [unit, size] =
		UNITS.find(([, s]) => Math.abs(seconds) >= s) ?? UNITS[UNITS.length - 1];
	return rtf.format(Math.round(seconds / size), unit);
}

export function RelativeTime({
	value,
	now = new Date(),
}: {
	value: string | Date;
	now?: Date;
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
						{formatRelative(date, now)}
					</time>
				</TooltipTrigger>
				<TooltipContent>{absolute}</TooltipContent>
			</Tooltip>
		</TooltipProvider>
	);
}
