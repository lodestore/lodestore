import { keepPreviousData } from "@tanstack/react-query";
import {
	ChevronRight,
	CircleX,
	Info,
	type LucideIcon,
	TriangleAlert,
} from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useListJobEvents } from "@/api/gen/endpoints";
import type { JobEventDto } from "@/api/gen/model";
import { CodeBlock } from "@/components/code-block";
import { formatClock } from "@/components/jobs/job-progress";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Skeleton } from "@/components/ui/skeleton";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

export const LOG_ANCHOR = "job-log";
export type LogFilter = "all" | "warnings" | "errors";

const POLL_MS = 1_000;
const NEAR_BOTTOM_PX = 24;
const FILTERS: LogFilter[] = ["all", "warnings", "errors"];
const LEVEL_OF: Record<Exclude<LogFilter, "all">, string> = {
	warnings: "warning",
	errors: "error",
};

const LEVELS: Record<string, { icon: LucideIcon; tone: string; word: string }> =
	{
		info: { icon: Info, tone: "text-neutral", word: copy.jobs.log.level.info },
		warning: {
			icon: TriangleAlert,
			tone: "text-warning",
			word: copy.jobs.log.level.warning,
		},
		error: {
			icon: CircleX,
			tone: "text-destructive",
			word: copy.jobs.log.level.error,
		},
	};

const KIND_LABEL: Record<string, string> = {
	changed_during_read: copy.jobs.log.warningKind.changed,
	unreadable: copy.jobs.log.warningKind.unreadable,
	vanished: copy.jobs.log.warningKind.disappeared,
	special_file: copy.jobs.log.warningKind.special,
};

type Row =
	| { type: "event"; event: JobEventDto }
	| { type: "hook"; seq: number; script: string; text: string };

function mergeEvents(known: JobEventDto[], incoming: JobEventDto[]) {
	const seen = new Set(known.map((e) => e.seq));
	const fresh = incoming.filter((e) => !seen.has(e.seq));
	return fresh.length === 0 ? known : [...known, ...fresh];
}

function toRows(events: JobEventDto[]) {
	const rows: Row[] = [];
	for (const event of events) {
		if (event.kind !== "hook_output") {
			rows.push({ type: "event", event });
			continue;
		}
		const script = event.script ?? "";
		const last = rows.at(-1);
		if (last?.type === "hook" && last.script === script) {
			last.text += `\n${event.message}`;
		} else {
			rows.push({ type: "hook", seq: event.seq, script, text: event.message });
		}
	}
	return rows;
}

function Message({ event }: { event: JobEventDto }) {
	const label = KIND_LABEL[event.kind];
	const path = event.path;
	const detail =
		event.message && event.message !== path ? event.message : undefined;
	if (!label) {
		return (
			<>
				{detail ?? event.message}
				{path && !event.message.includes(path) && (
					<>
						{" "}
						<code>{path}</code>
					</>
				)}
			</>
		);
	}
	return (
		<>
			<span className="font-semibold">{label}</span>
			{path && (
				<>
					{" "}
					<code>{path}</code>
				</>
			)}
			{detail && !(path && detail.includes(path)) && (
				<span className="text-muted-foreground"> {detail}</span>
			)}
		</>
	);
}

function EventRow({ event }: { event: JobEventDto }) {
	const level = LEVELS[event.level] ?? LEVELS.info;
	const Icon = level.icon;
	return (
		<div className="grid grid-cols-[7rem_1.5rem_minmax(0,1fr)] items-start gap-x-2 px-4 py-1 font-mono text-sm">
			<time
				dateTime={new Date(event.at).toISOString()}
				className="text-muted-foreground tabular-nums"
			>
				{formatClock(event.at)}
			</time>
			<span
				className={cn("flex size-6 items-center justify-center", level.tone)}
			>
				<Icon aria-hidden className="size-4" />
				<span className="sr-only">{level.word}</span>
			</span>
			<div className="min-w-0 wrap-anywhere">
				<Message event={event} />
			</div>
		</div>
	);
}

function HookRow({ script, text }: { script: string; text: string }) {
	const title = copy.jobs.log.hookOutput(script || copy.jobs.log.hookFallback);
	return (
		<Collapsible className="px-4 py-1">
			<CollapsibleTrigger asChild>
				<Button
					type="button"
					variant="ghost"
					size="sm"
					className="group/hook font-mono"
				>
					<ChevronRight
						aria-hidden
						className="transition-transform group-data-[state=open]/hook:rotate-90"
					/>
					{title}
				</Button>
			</CollapsibleTrigger>
			<CollapsibleContent className="pt-2">
				<CodeBlock text={text} object={title} />
			</CollapsibleContent>
		</Collapsible>
	);
}

export function JobLog({
	jobId,
	active,
	purged,
	filter,
	onFilterChange,
}: {
	jobId: string;
	active: boolean;
	purged: boolean;
	filter: LogFilter;
	onFilterChange: (filter: LogFilter) => void;
}) {
	const [events, setEvents] = useState<JobEventDto[]>([]);
	const [after, setAfter] = useState<number>();
	const [following, setFollowing] = useState(true);
	const box = useRef<HTMLDivElement>(null);

	const query = useListJobEvents(
		jobId,
		after === undefined ? undefined : { after },
		{
			query: {
				enabled: !purged,
				refetchInterval: active ? POLL_MS : false,
				refetchIntervalInBackground: false,
				placeholderData: keepPreviousData,
			},
		},
	);
	const { data, refetch } = query;

	useEffect(() => {
		if (!data || data.events.length === 0) return;
		setEvents((known) => mergeEvents(known, data.events));
		setAfter((cursor) => Math.max(cursor ?? 0, data.last_seq));
	}, [data]);

	const wasActive = useRef(active);
	useEffect(() => {
		if (wasActive.current && !active && !purged) void refetch();
		wasActive.current = active;
	}, [active, purged, refetch]);

	const limited = events.some((e) => e.kind === "log_limit");
	const rows = toRows(
		events.filter(
			(e) =>
				e.kind !== "log_limit" &&
				(filter === "all" || e.level === LEVEL_OF[filter]),
		),
	);

	const stuckToEnd = active && following;
	useLayoutEffect(() => {
		const el = box.current;
		if (stuckToEnd && el) el.scrollTop = el.scrollHeight;
	}, [stuckToEnd, rows.length, events.length, filter]);

	function onScroll(el: HTMLDivElement) {
		setFollowing(
			el.scrollHeight - el.scrollTop - el.clientHeight <= NEAR_BOTTOM_PX,
		);
	}

	function jumpToLatest() {
		const el = box.current;
		if (el) el.scrollTop = el.scrollHeight;
		setFollowing(true);
	}

	let body: React.ReactNode;
	if (purged) {
		body = (
			<p className="text-sm text-muted-foreground">{copy.jobs.log.expired}</p>
		);
	} else {
		body = (
			<div className="relative">
				<div
					ref={box}
					role="log"
					aria-live="off"
					aria-label={copy.jobs.log.title}
					tabIndex={0}
					onScroll={(e) => onScroll(e.currentTarget)}
					className="max-h-120 overflow-y-auto rounded-md bg-muted py-2 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
				>
					{query.isPending && events.length === 0 && (
						<div className="flex flex-col gap-2 px-4 py-2">
							<Skeleton className="h-4 w-3/4" />
							<Skeleton className="h-4 w-1/2" />
						</div>
					)}
					{!query.isPending && rows.length === 0 && !limited && (
						<p className="px-4 py-2 text-sm text-muted-foreground">
							{events.length === 0
								? copy.jobs.log.empty
								: copy.jobs.log.noMatches}
						</p>
					)}
					{rows.map((row) =>
						row.type === "hook" ? (
							<HookRow key={row.seq} script={row.script} text={row.text} />
						) : (
							<EventRow key={row.event.seq} event={row.event} />
						),
					)}
					{limited && (
						<p className="px-4 py-1 font-mono text-sm text-muted-foreground">
							{copy.jobs.log.limitReached}
						</p>
					)}
				</div>
				{active && !following && (
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="absolute right-4 bottom-2 bg-background"
						onClick={jumpToLatest}
					>
						{copy.jobs.log.jumpToLatest}
					</Button>
				)}
			</div>
		);
	}

	return (
		<Card id={LOG_ANCHOR}>
			<CardHeader className="flex flex-wrap items-center justify-between gap-4">
				<CardTitle>
					<h2>{copy.jobs.log.title}</h2>
				</CardTitle>
				{!purged && (
					<ToggleGroup
						type="single"
						variant="outline"
						size="lg"
						spacing={0}
						value={filter}
						aria-label={copy.jobs.log.filterLabel}
						onValueChange={(value) => {
							if (FILTERS.includes(value as LogFilter)) {
								onFilterChange(value as LogFilter);
							}
						}}
					>
						{FILTERS.map((name) => (
							<ToggleGroupItem
								key={name}
								value={name}
								className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
							>
								{copy.jobs.log.filter[name]}
							</ToggleGroupItem>
						))}
					</ToggleGroup>
				)}
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				{query.isError && events.length === 0 && !purged && (
					<Alert variant="destructive" role="alert">
						<AlertDescription className="flex items-center justify-between gap-4">
							{copy.jobs.log.loadFailed}
							<Button
								type="button"
								variant="secondary"
								size="sm"
								onClick={() => refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				)}
				{body}
			</CardContent>
		</Card>
	);
}
