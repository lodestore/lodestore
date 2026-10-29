import { Link } from "@tanstack/react-router";
import { Bell, CircleCheck, CircleQuestionMark, Database } from "lucide-react";
import { Fragment, type ReactNode, useId } from "react";
import { useListChannels } from "@/api/gen/endpoints";
import type {
	FleetHost,
	FleetStorage,
	FleetTotals,
	HostState,
} from "@/api/gen/model";
import { HOST_STATES } from "@/components/overview/host-state";
import { RelativeTime } from "@/components/relative-time";
import { Button } from "@/components/ui/button";
import {
	Popover,
	PopoverContent,
	PopoverHeader,
	PopoverTitle,
	PopoverTrigger,
} from "@/components/ui/popover";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { formatBytes, formatRatio } from "@/lib/format";
import { cn } from "@/lib/utils";

const text = copy.overview;

function StorageItem({ storage }: { storage: FleetStorage }) {
	const counted = storage.counted > 0 && storage.physical_bytes != null;
	const parts: ReactNode[] = [];
	if (counted) {
		parts.push(text.storage.stored(formatBytes(storage.physical_bytes ?? 0)));
		if ((storage.logical_bytes ?? 0) > 0 && storage.dedup_ratio != null) {
			parts.push(text.storage.dedup(formatRatio(storage.dedup_ratio)));
		}
		if (storage.counted_at != null) {
			parts.push(
				<>
					{text.storage.counted}
					<RelativeTime value={new Date(storage.counted_at)} />
				</>,
			);
		}
	}
	return (
		<p className="flex items-center gap-2">
			<Database aria-hidden className="size-4 shrink-0" />
			<span>
				{counted
					? parts.map((part, i) => (
							<Fragment key={i}>
								{i > 0 && text.storage.sep}
								{part}
							</Fragment>
						))
					: text.storage.notCounted}
			</span>
		</p>
	);
}

function NotifyHint() {
	const channels = useListChannels();
	const hint = copy.notifications.overviewHint;
	if (channels.data?.length !== 0) return null;
	return (
		<div className="flex flex-wrap items-center gap-2">
			<Bell aria-hidden className="size-4 shrink-0" />
			{hint.text}
			<Button variant="outline" size="sm" asChild>
				<Link to="/settings/notifications">{hint.link}</Link>
			</Button>
		</div>
	);
}

function StateHelp() {
	const titleId = useId();
	return (
		<Popover>
			<PopoverTrigger asChild>
				<Button variant="outline" size="icon-sm" aria-label={text.legend.help}>
					<CircleQuestionMark aria-hidden />
				</Button>
			</PopoverTrigger>
			<PopoverContent aria-labelledby={titleId} collisionPadding={16}>
				<PopoverHeader>
					<PopoverTitle id={titleId}>{text.legend.help}</PopoverTitle>
				</PopoverHeader>
				<dl className="flex flex-col gap-4">
					{HOST_STATES.map((s) => (
						<div key={s.state} className="flex flex-col gap-1">
							<dt className="flex items-center gap-2 font-semibold">
								<s.icon aria-hidden className={cn("size-4", s.tone)} />
								{s.label}
							</dt>
							<dd className="text-muted-foreground">
								{text.stateHelp[s.state]}
							</dd>
						</div>
					))}
				</dl>
			</PopoverContent>
		</Popover>
	);
}

function SummaryFacts({ storage }: { storage: FleetStorage }) {
	return (
		<div className="flex flex-wrap items-center gap-x-6 gap-y-2 text-sm text-muted-foreground">
			<StorageItem storage={storage} />
			<NotifyHint />
		</div>
	);
}

export function FleetSummary({
	totals,
	storage,
	hosts,
	filter,
	onFilter,
}: {
	totals: FleetTotals;
	storage: FleetStorage;
	hosts: FleetHost[];
	filter?: HostState;
	onFilter: (state: HostState | undefined) => void;
}) {
	const attention = totals.hosts - totals.healthy;
	const shown = HOST_STATES.filter(
		(s) => totals[s.state] > 0 || filter === s.state,
	);

	if (attention === 0) {
		const newest = Math.max(0, ...hosts.map((h) => h.last_success_at ?? 0));
		return (
			<div className="flex flex-col gap-4">
				<div className="flex flex-col gap-2">
					<h2 className="flex items-center gap-4 text-display font-semibold">
						<CircleCheck aria-hidden className="size-8 text-success" />
						{text.verdict.allProtected(totals.hosts)}
					</h2>
					{newest > 0 && (
						<p className="text-muted-foreground">
							{text.verdict.newest[0]}
							<RelativeTime value={new Date(newest)} />
							{text.verdict.newest[1]}
						</p>
					)}
				</div>
				<SummaryFacts storage={storage} />
			</div>
		);
	}

	const worst = HOST_STATES.find(
		(s) => totals[s.state] > 0 && s.state !== "healthy",
	);
	const WorstIcon = worst?.icon;
	return (
		<div className="flex flex-col gap-4">
			<h2 className="flex items-center gap-2 text-2xl font-semibold">
				{WorstIcon && (
					<WorstIcon
						aria-hidden
						className={cn("size-6 shrink-0", worst?.tone)}
					/>
				)}
				{text.verdict.attention(attention, totals.hosts)}
			</h2>
			<div aria-hidden className="flex h-2 gap-1">
				{shown
					.filter((s) => totals[s.state] > 0)
					.map((s) => (
						<div
							key={s.state}
							className={cn("rounded-full", s.segment)}
							style={{ flexGrow: totals[s.state] }}
						/>
					))}
			</div>
			<div
				role="group"
				aria-label={text.legend.label}
				className="flex flex-wrap gap-2"
			>
				{shown.map((s) => (
					<Tooltip key={s.state}>
						<TooltipTrigger asChild>
							<Button
								variant="outline"
								size="sm"
								aria-pressed={filter === s.state}
								className={cn(
									filter === s.state &&
										"border-primary-strong bg-primary-subtle",
								)}
								onClick={() =>
									onFilter(filter === s.state ? undefined : s.state)
								}
							>
								<s.icon aria-hidden className={s.tone} />
								{s.label}
								<span className="tabular-nums">{totals[s.state]}</span>
							</Button>
						</TooltipTrigger>
						<TooltipContent>{text.stateHelp[s.state]}</TooltipContent>
					</Tooltip>
				))}
				<StateHelp />
			</div>
			<SummaryFacts storage={storage} />
		</div>
	);
}
