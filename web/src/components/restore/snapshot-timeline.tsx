import { useInfiniteQuery } from "@tanstack/react-query";
import { type KeyboardEvent, memo, useId, useState } from "react";
import {
	getListRestoreSnapshotsQueryKey,
	listRestoreSnapshots,
	useListRestoreHosts,
} from "@/api/gen/endpoints";
import type { RestoreSnapshot } from "@/api/gen/model";
import {
	isRebuilding,
	useCatalogStatus,
} from "@/components/repos/catalog-card";
import { SuspiciousBadge } from "@/components/snapshots/suspicious-badge";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import {
	formatBytes,
	formatClock,
	formatCount,
	formatDateTime,
	formatDay,
} from "@/lib/format";
import { shortId, snapshotSource } from "@/lib/snapshot";
import { cn } from "@/lib/utils";

const PAGE_SIZE = 50;
const REFRESH_MS = 15_000;
const SKELETON_STOPS = 6;
const MOVES = ["ArrowDown", "ArrowUp", "Home", "End"];

export function useSnapshotTimeline(hostKey?: string) {
	return useInfiniteQuery({
		queryKey: [...getListRestoreSnapshotsQueryKey(), "timeline", hostKey],
		enabled: hostKey !== undefined,
		initialPageParam: undefined as string | undefined,
		queryFn: ({ pageParam, signal }) =>
			listRestoreSnapshots(
				{ host: hostKey, before: pageParam, limit: PAGE_SIZE },
				{ signal },
			),
		getNextPageParam: (last) => last.next ?? undefined,
		refetchInterval: REFRESH_MS,
		refetchIntervalInBackground: false,
		select: (data) => ({
			items: data.pages.flatMap((page) => page.items),
			total: data.pages[0]?.total ?? 0,
		}),
	});
}

function groupByDay(items: RestoreSnapshot[]) {
	const now = Date.now();
	const groups: { day: string; items: RestoreSnapshot[] }[] = [];
	for (const item of items) {
		const day = formatDay(item.time, now);
		const last = groups.at(-1);
		if (last?.day === day) last.items.push(item);
		else groups.push({ day, items: [item] });
	}
	return groups;
}

function moveFocus(event: KeyboardEvent<HTMLButtonElement>) {
	if (!MOVES.includes(event.key)) return;
	const stops = Array.from(
		event.currentTarget
			.closest("[data-timeline]")
			?.querySelectorAll<HTMLButtonElement>("button[data-stop]") ?? [],
	);
	const at = stops.indexOf(event.currentTarget);
	const next = {
		ArrowDown: Math.min(at + 1, stops.length - 1),
		ArrowUp: Math.max(at - 1, 0),
		Home: 0,
		End: stops.length - 1,
	}[event.key];
	event.preventDefault();
	stops[next as number]?.focus();
}

function Stop({
	snapshot,
	selected,
	tabStop,
	onSelect,
	onFocus,
}: {
	snapshot: RestoreSnapshot;
	selected: boolean;
	tabStop: boolean;
	onSelect: () => void;
	onFocus: () => void;
}) {
	const id = useId();
	const described = [
		snapshot.suspicious && `${id}-suspicious`,
		snapshot.incomplete && `${id}-incomplete`,
		selected && `${id}-selected`,
	]
		.filter(Boolean)
		.join(" ");
	return (
		<button
			type="button"
			data-stop
			tabIndex={tabStop ? 0 : -1}
			aria-current={selected ? "true" : undefined}
			aria-label={copy.restore.timeline.stopLabel(
				shortId(snapshot.id),
				formatDateTime(snapshot.time),
			)}
			aria-describedby={described || undefined}
			onClick={onSelect}
			onFocus={onFocus}
			onKeyDown={moveFocus}
			className={cn(
				"relative flex min-h-16 w-full items-center gap-4 px-4 py-2 text-left outline-none transition-colors before:absolute before:inset-y-0 before:left-6 before:w-0.5 before:-translate-x-1/2 before:bg-border hover:bg-muted focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring",
				selected && "bg-primary-subtle hover:bg-primary-subtle",
			)}
		>
			<span
				aria-hidden
				className={cn(
					"relative z-10 size-4 shrink-0 rounded-full border-2 bg-card",
					selected
						? "border-primary-strong bg-primary-strong"
						: "border-border",
				)}
			/>
			<span className="flex min-w-0 flex-col">
				<span className="flex flex-wrap items-center gap-x-2 gap-y-1">
					<span className="font-semibold tabular-nums">
						{formatClock(snapshot.time)}
					</span>
					{snapshot.suspicious && (
						<span id={`${id}-suspicious`} className="flex">
							<SuspiciousBadge snapshot={snapshot} plain />
						</span>
					)}
					{snapshot.incomplete && (
						<span id={`${id}-incomplete`} className="flex">
							<StatusBadge status="warning">
								{copy.restore.header.incomplete}
							</StatusBadge>
						</span>
					)}
					{selected && (
						<span id={`${id}-selected`} className="sr-only">
							{copy.restore.timeline.selected}
						</span>
					)}
				</span>
				<span
					className={cn(
						"text-sm text-muted-foreground",
						snapshot.policy_deleted ? "wrap-anywhere" : "truncate",
					)}
				>
					{snapshotSource(snapshot)}
					{snapshot.policy_deleted &&
						` · ${copy.snapshots.policyDeletedInline}`}{" "}
					· <span className="font-mono">{shortId(snapshot.id)}</span> ·{" "}
					<span className="tabular-nums">
						{formatBytes(snapshot.stats.bytes_total)}
					</span>
				</span>
			</span>
		</button>
	);
}

export const SnapshotTimeline = memo(function SnapshotTimeline({
	hostKey,
	selected,
	onSelect,
	scrollClassName,
}: {
	hostKey: string;
	selected?: string;
	onSelect: (id: string) => void;
	scrollClassName?: string;
}) {
	const timeline = useSnapshotTimeline(hostKey);
	const repositoryId = useListRestoreHosts().data?.find(
		(host) => host.key === hostKey,
	)?.repository_id;
	const rebuilding = useCatalogStatus().data?.some(
		(status) => status.repository_id === repositoryId && isRebuilding(status),
	);
	const [focused, setFocused] = useState<string>();
	const items = timeline.data?.items ?? [];
	const total = timeline.data?.total ?? 0;
	const tabStop = [focused, selected].find((id) =>
		items.some((item) => item.id === id),
	);

	let body: React.ReactNode;
	if (timeline.isError && timeline.data === undefined) {
		body = (
			<Alert variant="destructive" role="alert" className="m-4 w-auto">
				<AlertDescription className="flex flex-col items-start gap-2">
					{copy.restore.timeline.loadFailed}
					<Button
						type="button"
						variant="secondary"
						size="sm"
						onClick={() => timeline.refetch()}
					>
						{copy.errors.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	} else if (timeline.data === undefined) {
		body = (
			<div aria-hidden>
				{Array.from({ length: SKELETON_STOPS }, (_, i) => (
					<div key={i} className="flex h-16 items-center gap-4 px-4">
						<Skeleton className="size-4 rounded-full" />
						<div className="flex flex-col gap-2">
							<Skeleton className="h-4 w-16" />
							<Skeleton className="h-4 w-40" />
						</div>
					</div>
				))}
			</div>
		);
	} else {
		body = (
			<div data-timeline>
				{groupByDay(items).map(({ day, items: group }) => (
					<section key={day}>
						<h3 className="sticky top-0 z-20 flex h-8 items-center bg-card px-4 text-sm font-semibold text-muted-foreground">
							{day}
						</h3>
						<ul>
							{group.map((snapshot) => (
								<li key={snapshot.id}>
									<Stop
										snapshot={snapshot}
										selected={snapshot.id === selected}
										tabStop={snapshot.id === (tabStop ?? items[0]?.id)}
										onSelect={() => onSelect(snapshot.id)}
										onFocus={() => setFocused(snapshot.id)}
									/>
								</li>
							))}
						</ul>
					</section>
				))}
				<div className="flex flex-col items-start gap-2 p-4">
					{timeline.hasNextPage && (
						<Button
							type="button"
							variant="outline"
							size="sm"
							disabled={timeline.isFetchingNextPage}
							onClick={() => timeline.fetchNextPage()}
						>
							{timeline.isFetchingNextPage && <Spinner />}
							{copy.restore.timeline.showOlder}
						</Button>
					)}
					<p className="text-sm text-muted-foreground">
						{copy.restore.timeline.showing(items.length, total)}
					</p>
				</div>
			</div>
		);
	}

	return (
		<div className="flex flex-col overflow-hidden rounded-lg border bg-card">
			<div className="border-b px-4 py-2">
				<h2 className="flex items-baseline gap-2 text-sm font-semibold">
					{copy.restore.timeline.title}{" "}
					{timeline.data && (
						<span className="font-normal text-muted-foreground tabular-nums">
							{formatCount(total)}
						</span>
					)}
				</h2>
				{rebuilding && (
					<p className="text-sm text-muted-foreground">
						{copy.restore.timeline.rebuilding}
					</p>
				)}
			</div>
			<ScrollArea
				className={cn(
					"[&_[data-slot=scroll-area-viewport]>div]:block!",
					scrollClassName,
				)}
			>
				{body}
			</ScrollArea>
		</div>
	);
});
