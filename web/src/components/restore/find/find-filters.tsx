import { Calendar, ChevronDown } from "lucide-react";
import { useState } from "react";
import { formatMonthDay } from "@/components/restore/find/dates";
import { HostPicker } from "@/components/restore/host-picker";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Popover,
	PopoverContent,
	PopoverTrigger,
} from "@/components/ui/popover";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { copy } from "@/copy";

const DAY = 86_400_000;
const PRESETS = { week: 7, month: 30 } as const;

type Period = "any" | "week" | "month" | "custom";
export type FindScope = {
	hosts?: string;
	labels?: string;
	since?: number;
	until?: number;
};

const list = (value: string | undefined) =>
	value?.split(",").filter(Boolean) ?? [];

function startOfDay(ms: number) {
	const date = new Date(ms);
	return new Date(
		date.getFullYear(),
		date.getMonth(),
		date.getDate(),
	).getTime();
}

const presetSince = (days: number) => startOfDay(Date.now() - days * DAY);

function periodOf({ since, until }: FindScope): Period {
	if (since === undefined && until === undefined) return "any";
	if (until === undefined) {
		for (const [name, days] of Object.entries(PRESETS)) {
			if (since === presetSince(days)) return name as Period;
		}
	}
	return "custom";
}

function inputDate(ms: number | undefined) {
	if (ms === undefined) return "";
	const date = new Date(ms);
	const pad = (n: number) => String(n).padStart(2, "0");
	return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

function parseDate(value: string, end: boolean) {
	const [year, month, day] = value.split("-").map(Number);
	if (!year || !month || !day) return undefined;
	return (
		new Date(year, month - 1, day + (end ? 1 : 0)).getTime() - (end ? 1 : 0)
	);
}

function periodLabel(period: Period, { since, until }: FindScope) {
	if (period === "any") return copy.find.filters.anyTime;
	if (period !== "custom") return copy.find.filters.periods[period];
	if (since !== undefined && until !== undefined) {
		return copy.find.range(formatMonthDay(since), formatMonthDay(until));
	}
	if (since !== undefined) {
		return `${copy.find.filters.from} ${formatMonthDay(since)}`;
	}
	if (until !== undefined) {
		return `${copy.find.filters.to} ${formatMonthDay(until)}`;
	}
	return copy.find.filters.periods.custom;
}

function PeriodPicker({
	scope,
	onChange,
}: {
	scope: FindScope;
	onChange: (patch: Partial<FindScope>) => void;
}) {
	const [open, setOpen] = useState(false);
	const [custom, setCustom] = useState(periodOf(scope) === "custom");
	const period = custom ? "custom" : periodOf(scope);

	function choose(next: string) {
		setCustom(next === "custom");
		if (next === "custom") return;
		setOpen(false);
		onChange({
			since: next === "any" ? undefined : presetSince(PRESETS[next as "week"]),
			until: undefined,
		});
	}

	return (
		<Popover open={open} onOpenChange={setOpen}>
			<PopoverTrigger asChild>
				<Button type="button" variant="outline" size="sm">
					<Calendar aria-hidden />
					{periodLabel(period, scope)}
					<ChevronDown aria-hidden />
				</Button>
			</PopoverTrigger>
			<PopoverContent align="start" className="w-64 gap-4">
				<RadioGroup
					value={period}
					onValueChange={choose}
					aria-label={copy.find.filters.anyTime}
				>
					{(["any", "week", "month", "custom"] as const).map((name) => (
						<Label key={name} className="gap-2 font-normal">
							<RadioGroupItem value={name} />
							{name === "any"
								? copy.find.filters.anyTime
								: copy.find.filters.periods[name]}
						</Label>
					))}
				</RadioGroup>
				{period === "custom" && (
					<div className="flex flex-col gap-2">
						<Label className="flex-col items-start gap-1">
							{copy.find.filters.from}
							<Input
								type="date"
								value={inputDate(scope.since)}
								max={inputDate(scope.until)}
								onChange={(event) =>
									onChange({ since: parseDate(event.target.value, false) })
								}
							/>
						</Label>
						<Label className="flex-col items-start gap-1">
							{copy.find.filters.to}
							<Input
								type="date"
								value={inputDate(scope.until)}
								min={inputDate(scope.since)}
								onChange={(event) =>
									onChange({ until: parseDate(event.target.value, true) })
								}
							/>
						</Label>
					</div>
				)}
			</PopoverContent>
		</Popover>
	);
}

function toggle(values: string[], value: string) {
	const next = values.includes(value)
		? values.filter((item) => item !== value)
		: [...values, value];
	return next.length > 0 ? next.join(",") : undefined;
}

export function FindFilters({
	scope,
	onChange,
}: {
	scope: FindScope;
	onChange: (patch: Partial<FindScope>) => void;
}) {
	const hosts = list(scope.hosts);
	const labels = list(scope.labels);
	return (
		<div className="flex flex-wrap items-center gap-2">
			<HostPicker
				selected={hosts}
				selectedLabels={labels}
				onToggleHost={(key) => onChange({ hosts: toggle(hosts, key) })}
				onToggleLabel={(label) => onChange({ labels: toggle(labels, label) })}
			/>
			<PeriodPicker scope={scope} onChange={onChange} />
			<p className="w-full text-sm text-muted-foreground md:ml-auto md:w-auto">
				{copy.find.scope}
			</p>
		</div>
	);
}
