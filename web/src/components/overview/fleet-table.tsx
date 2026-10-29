import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal, X } from "lucide-react";
import {
	Fragment,
	type KeyboardEvent,
	type ReactElement,
	useLayoutEffect,
	useRef,
	useState,
} from "react";
import { toast } from "sonner";
import { getListJobsQueryKey, useRunHost } from "@/api/gen/endpoints";
import type {
	AlertDto,
	AlertType,
	FleetHost,
	HostState,
} from "@/api/gen/model";
import { Truncated } from "@/components/hosts/hosts-table";
import { formatRelative, RelativeTime } from "@/components/relative-time";
import { SortButton } from "@/components/sort-button";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSub,
	DropdownMenuSubContent,
	DropdownMenuSubTrigger,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { copy } from "@/copy";
import { formatBytes, formatDateTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { order } from "./active-alerts";
import { HOST_STATES, HostStateBadge } from "./host-state";
import { RunLegend, RunStrip } from "./run-strip";
import { SilenceItems, useSilence } from "./silence-menu";

const PAGE_SIZE = 50;
const CELL = "px-2 @min-[680px]:px-4";
const SKELETON_ROWS = [0, 1, 2, 3, 4];
const SHOW_RUNS = "hidden @min-[1040px]:table-cell";
const SHOW_LAST = "hidden @min-[560px]:table-cell";
const SHOW_SIZE = "hidden @min-[840px]:table-cell";
const SHOW_WIDE = "hidden @min-[1240px]:table-cell";
const PRESENTED: Partial<Record<HostState, AlertType>> = {
	failed: "backup_failed",
	overdue: "no_recent_backup",
	offline: "agent_offline",
};
const STATE_RANK: Record<HostState, number> = {
	failed: 0,
	overdue: 1,
	offline: 2,
	not_protected: 3,
	healthy: 4,
};

const MOVES = ["ArrowDown", "ArrowUp", "Home", "End"];

type SortKey = "host" | "lastBackup";
type Sort = { key: SortKey; dir: "asc" | "desc" } | null;

const text = copy.overview.table;
const alertText = copy.overview.alerts;
const ACTIONS = "w-px px-2 whitespace-nowrap @min-[680px]:px-4";

const byName = (a: FleetHost, b: FleetHost) =>
	a.hostname.localeCompare(b.hostname, undefined, {
		sensitivity: "base",
		numeric: true,
	});

const triage = (a: FleetHost, b: FleetHost) =>
	Number(a.silenced) - Number(b.silenced) ||
	STATE_RANK[a.state] - STATE_RANK[b.state] ||
	byName(a, b);

function sortHosts(hosts: FleetHost[], sort: Sort, byDefault = byName) {
	if (sort === null) return [...hosts].sort(byDefault);
	const sign = sort.dir === "asc" ? 1 : -1;
	return [...hosts].sort((a, b) => {
		const primary =
			sort.key === "host"
				? byName(a, b)
				: (a.last_success_at ?? -1) - (b.last_success_at ?? -1);
		return sign * primary || byName(a, b);
	});
}

function reasonOf(host: FleetHost, alerts: AlertDto[]) {
	const pair = host.worst;
	let base = "";
	if ((host.state === "failed" || host.state === "overdue") && pair) {
		base =
			pair.problem === "overdue"
				? text.statusOverdue(pair.policy, pair.age_secs, pair.limit_secs)
				: text.statusFailed(pair.policy, pair.error);
	} else if (host.state === "offline") {
		base = text.reason.offline(
			host.agent_last_seen_at == null
				? undefined
				: formatRelative(new Date(host.agent_last_seen_at), new Date()),
		);
	} else if (host.state === "not_protected") {
		base = host.has_policy ? text.reason.noSuccessYet : text.reason.noPolicy;
	}
	const others = [...new Set(alerts.map((a) => a.type))]
		.filter((type) => type !== PRESENTED[host.state])
		.map((type) => copy.notifications.alertTypes[type]);
	return [base, ...others].filter(Boolean).join(text.reason.separator);
}

function HostSubline({ host }: { host: FleetHost }) {
	const labels = host.labels.map((l) => `${l.key}=${l.value}`);
	if (labels.length === 0) {
		return (
			<Truncated
				text={host.platform}
				className="text-sm text-muted-foreground"
			/>
		);
	}
	const full = labels.join(" ");
	const cls = "font-mono text-sm text-muted-foreground";
	if (labels.length === 1) return <Truncated text={full} className={cls} />;
	const rest = labels.length - 2;
	return (
		<>
			<Truncated text={full} className={cn(cls, "@min-[1040px]:hidden")}>
				{`${labels[0]} ${text.moreLabels(labels.length - 1)}`}
			</Truncated>
			<Truncated text={full} className={cn(cls, "hidden @min-[1040px]:block")}>
				{rest > 0
					? `${labels.slice(0, 2).join(" ")} ${text.moreLabels(rest)}`
					: full}
			</Truncated>
		</>
	);
}

function useRunAll(host: FleetHost) {
	const queryClient = useQueryClient();
	const run = useRunHost();
	return () =>
		run.mutate(
			{ id: host.id, data: { policies: null } },
			{
				onSuccess: ({ jobs }) => {
					const [first] = jobs;
					toast.success(
						jobs.length === 1
							? first.created
								? copy.jobs.run.queuedOne(host.hostname)
								: copy.jobs.run.allAlreadyQueued(host.hostname)
							: copy.jobs.run.queuedMany(jobs.length),
					);
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
				},
				onError: (error) =>
					toast.error(
						copy.errors.submit(copy.hostBackup.startFailed, error.message),
					),
			},
		);
}

function RowActions({ host, tab }: { host: FleetHost; tab: number }) {
	const navigate = useNavigate();
	const { silence, resume } = useSilence(host.id, host.hostname);
	const runAll = useRunAll(host);
	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					tabIndex={tab}
					data-focus="actions"
					aria-label={text.actionsFor(host.hostname)}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem
					onSelect={() =>
						void navigate({ to: "/hosts/$id", params: { id: host.id } })
					}
				>
					{text.viewHost}
				</DropdownMenuItem>
				<DropdownMenuSub>
					<DropdownMenuSubTrigger>
						{copy.overview.alerts.silence.trigger}
					</DropdownMenuSubTrigger>
					<DropdownMenuSubContent>
						<SilenceItems
							silenced={host.silenced}
							onSilence={silence}
							onResume={resume}
						/>
					</DropdownMenuSubContent>
				</DropdownMenuSub>
				{host.has_policy && (
					<DropdownMenuItem onSelect={runAll}>
						{text.runBackup}
					</DropdownMenuItem>
				)}
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

function FixButton({ host, tab }: { host: FleetHost; tab: number }) {
	const pair = host.worst;
	let target: ReactElement;
	if ((host.state === "failed" || host.state === "overdue") && pair) {
		target = (
			<Link
				to="/policies/$name"
				params={{ name: pair.policy }}
				aria-label={text.fixFor.policy(pair.policy, host.hostname)}
			>
				{text.fix.policy}
			</Link>
		);
	} else if (host.state === "offline") {
		target = (
			<Link
				to="/hosts/$id"
				params={{ id: host.id }}
				aria-label={text.fixFor.agent(host.hostname)}
			>
				{text.fix.agent}
			</Link>
		);
	} else if (host.state === "not_protected" && !host.has_policy) {
		target = (
			<Link to="/policies/new" aria-label={text.fixFor.add(host.hostname)}>
				{text.fix.add}
			</Link>
		);
	} else {
		target = (
			<Link
				to="/hosts/$id"
				params={{ id: host.id }}
				aria-label={text.fixFor.host(host.hostname)}
			>
				{text.fix.host}
			</Link>
		);
	}
	return (
		<Button
			variant="outline"
			size="sm"
			tabIndex={tab}
			className="hidden @min-[480px]:inline-flex"
			asChild
		>
			{target}
		</Button>
	);
}

function FleetRow({
	host,
	alerts,
	tab,
}: {
	host: FleetHost;
	alerts: AlertDto[];
	tab: number;
}) {
	const navigate = useNavigate();
	const reason = reasonOf(host, alerts);
	const since =
		alerts.length > 0 ? Math.min(...alerts.map((a) => a.started_at)) : null;
	return (
		<TableRow
			data-host-row={host.id}
			className="h-16 cursor-pointer has-focus-visible:bg-muted/50"
			onClick={(e) => {
				const target = e.target as HTMLElement;
				if (!target.closest("a,button,[role=menuitem]")) {
					void navigate({ to: "/hosts/$id", params: { id: host.id } });
				}
			}}
		>
			<TableCell
				className={cn(
					CELL,
					"max-w-40 @min-[840px]:max-w-48 @min-[1040px]:max-w-72",
				)}
			>
				<Truncated
					text={host.hostname}
					className="w-fit max-w-full rounded-sm has-focus-visible:outline-2 has-focus-visible:outline-offset-2"
				>
					<Link
						to="/hosts/$id"
						params={{ id: host.id }}
						data-focus="name"
						tabIndex={tab}
						className="rounded-sm text-base font-semibold focus-visible:outline-hidden"
					>
						{host.hostname}
					</Link>
				</Truncated>
				<HostSubline host={host} />
			</TableCell>
			<TableCell className={cn(CELL, "w-full max-w-0 min-w-32")}>
				<div className="flex items-center gap-2 overflow-hidden">
					<HostStateBadge state={host.state} />
					{host.silenced && (
						<StatusBadge status="silenced">
							<span className="@min-[840px]:hidden">
								{copy.overview.status.silenced}
							</span>
							<span className="hidden @min-[840px]:inline">
								{host.silenced_until == null
									? alertText.silence.badgeForever
									: alertText.silence.badge(
											formatDateTime(host.silenced_until),
										)}
							</span>
						</StatusBadge>
					)}
					{since != null && !host.silenced && host.state !== "offline" && (
						<span className="hidden text-sm whitespace-nowrap text-muted-foreground @min-[840px]:inline">
							{alertText.since}
							<RelativeTime value={new Date(since)} tabIndex={tab} />
						</span>
					)}
				</div>
				{reason !== "" && (
					<Truncated text={reason} className="text-sm text-muted-foreground" />
				)}
			</TableCell>
			<TableCell className={cn(CELL, SHOW_RUNS)}>
				<RunStrip runs={host.recent_runs} />
			</TableCell>
			<TableCell
				className={cn(
					CELL,
					SHOW_LAST,
					host.state === "overdue" && "font-semibold",
				)}
			>
				{host.last_success_at != null ? (
					<RelativeTime value={new Date(host.last_success_at)} tabIndex={tab} />
				) : (
					text.never
				)}
			</TableCell>
			<TableCell className={cn(CELL, "tabular-nums", SHOW_SIZE)}>
				{host.last_snapshot
					? formatBytes(host.last_snapshot.bytes_total)
					: copy.common.dash}
			</TableCell>
			<TableCell className={cn(CELL, "tabular-nums", SHOW_WIDE)}>
				{host.last_snapshot
					? formatBytes(host.last_snapshot.bytes_added)
					: copy.common.dash}
			</TableCell>
			<TableCell className={cn(CELL, SHOW_WIDE)}>
				<StatusBadge status={host.agent_online ? "online" : "offline"} />
				{!host.agent_online && host.agent_last_seen_at != null && (
					<p className="text-sm whitespace-nowrap text-muted-foreground">
						<RelativeTime
							value={new Date(host.agent_last_seen_at)}
							tabIndex={tab}
						/>
					</p>
				)}
			</TableCell>
			<TableCell className={ACTIONS} onClick={(e) => e.stopPropagation()}>
				<div className="flex items-center justify-end gap-2">
					{host.state !== "healthy" && <FixButton host={host} tab={tab} />}
					<RowActions host={host} tab={tab} />
				</div>
			</TableCell>
		</TableRow>
	);
}

function moveBetweenRows(e: KeyboardEvent<HTMLTableSectionElement>) {
	const target = e.target as HTMLElement;
	if (
		e.defaultPrevented ||
		e.altKey ||
		e.ctrlKey ||
		e.metaKey ||
		e.shiftKey ||
		!MOVES.includes(e.key) ||
		!e.currentTarget.contains(target) ||
		(e.key === "ArrowDown" && target.closest('[data-focus="actions"]'))
	) {
		return;
	}
	const current = target.closest("[data-host-row]");
	if (!current) return;
	const rows = Array.from(
		e.currentTarget.querySelectorAll<HTMLElement>("[data-host-row]"),
	);
	const last = rows.length - 1;
	const at = rows.indexOf(current as HTMLElement);
	const next =
		e.key === "Home"
			? 0
			: e.key === "End"
				? last
				: Math.min(last, Math.max(0, at + (e.key === "ArrowDown" ? 1 : -1)));
	e.preventDefault();
	rows[next].querySelector<HTMLElement>('[data-focus="name"]')?.focus();
}

export function FleetTable({
	hosts,
	alerts,
	filter,
	onClearFilter,
}: {
	hosts?: FleetHost[];
	alerts?: AlertDto[];
	filter?: HostState;
	onClearFilter: () => void;
}) {
	const [sort, setSort] = useState<Sort>(null);
	const [page, setPage] = useState(0);
	const [active, setActive] = useState<string | null>(null);
	const focused = useRef<{ id: string; key: string } | null>(null);

	const found = (hosts ?? []).filter(
		(h) => filter === undefined || h.state === filter,
	);
	const attention = sortHosts(
		found.filter((h) => h.state !== "healthy"),
		sort,
		triage,
	);
	const quiet = sortHosts(
		found.filter((h) => h.state === "healthy"),
		sort,
	);
	const rows = [...attention, ...quiet];
	const counts = { attention: attention.length, protected: quiet.length };
	const alertsByHost = new Map<string, AlertDto[]>();
	for (const alert of [...(alerts ?? [])].sort(order)) {
		if (!alert.host) continue;
		alertsByHost.set(alert.host.id, [
			...(alertsByHost.get(alert.host.id) ?? []),
			alert,
		]);
	}
	const lastPage = Math.max(0, Math.ceil(rows.length / PAGE_SIZE) - 1);
	const current = Math.min(page, lastPage);
	const visible = rows.slice(current * PAGE_SIZE, (current + 1) * PAGE_SIZE);
	const visibleIds = visible.map((h) => h.id).join(",");
	const activeId = visible.some((h) => h.id === active)
		? active
		: visible[0]?.id;

	useLayoutEffect(() => {
		const target = focused.current;
		if (
			visibleIds === "" ||
			!target ||
			document.activeElement !== document.body
		) {
			return;
		}
		for (const row of document.querySelectorAll<HTMLElement>(
			"[data-host-row]",
		)) {
			if (row.dataset.hostRow === target.id) {
				row.querySelector<HTMLElement>(`[data-focus="${target.key}"]`)?.focus();
			}
		}
	}, [visibleIds]);

	const sortBy = (key: SortKey) => {
		setPage(0);
		setSort((prev) =>
			prev?.key === key
				? { key, dir: prev.dir === "asc" ? "desc" : "asc" }
				: { key, dir: key === "lastBackup" ? "desc" : "asc" },
		);
	};
	const ariaSort = (key: SortKey) =>
		sort?.key === key
			? sort.dir === "asc"
				? "ascending"
				: "descending"
			: undefined;
	const clear = () => {
		setPage(0);
		onClearFilter();
	};

	return (
		<section className="@container flex flex-col gap-4">
			<div className="flex flex-wrap items-center gap-4">
				<h2 className="text-base font-semibold">{text.heading}</h2>
				{filter !== undefined && (
					<Button
						variant="outline"
						size="sm"
						onClick={() => {
							setPage(0);
							onClearFilter();
						}}
					>
						{copy.overview.filter.showing(
							HOST_STATES.find((s) => s.state === filter)?.label ?? filter,
						)}
						<X aria-hidden />
						<span className="sr-only">{text.clearFilter}</span>
					</Button>
				)}
			</div>

			<div className="@container overflow-hidden rounded-lg border bg-card">
				<Table>
					<TableHeader className="bg-muted">
						<TableRow>
							<TableHead className={CELL} aria-sort={ariaSort("host")}>
								<SortButton
									label={text.columns.host}
									active={sort?.key === "host"}
									dir={sort?.dir ?? "asc"}
									onClick={() => sortBy("host")}
								/>
							</TableHead>
							<TableHead className={cn(CELL, "w-full")}>
								{text.columns.status}
							</TableHead>
							<TableHead className={cn(CELL, SHOW_RUNS)}>
								{copy.hostBackup.runs.label}
							</TableHead>
							<TableHead
								className={cn(CELL, SHOW_LAST, "whitespace-nowrap")}
								aria-sort={ariaSort("lastBackup")}
							>
								<SortButton
									className="text-left"
									label={text.columns.lastBackup}
									active={sort?.key === "lastBackup"}
									dir={sort?.dir ?? "desc"}
									onClick={() => sortBy("lastBackup")}
								/>
							</TableHead>
							<TableHead className={cn(CELL, SHOW_SIZE)}>
								{text.columns.size}
							</TableHead>
							<TableHead className={cn(CELL, SHOW_WIDE)}>
								{text.columns.added}
							</TableHead>
							<TableHead className={cn(CELL, SHOW_WIDE)}>
								{text.columns.agent}
							</TableHead>
							<TableHead className={ACTIONS}>
								<span className="sr-only">{text.columns.actions}</span>
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody
						onKeyDown={moveBetweenRows}
						onFocus={(e) => {
							const row = (e.target as HTMLElement).closest<HTMLElement>(
								"[data-host-row]",
							);
							const key = (e.target as HTMLElement).dataset.focus;
							focused.current = row?.dataset.hostRow
								? { id: row.dataset.hostRow, key: key ?? "name" }
								: null;
							if (row?.dataset.hostRow) setActive(row.dataset.hostRow);
						}}
					>
						{hosts === undefined &&
							SKELETON_ROWS.map((i) => (
								<TableRow
									key={i}
									aria-hidden
									data-skeleton-row
									className="h-16"
								>
									<TableCell className={CELL}>
										<Skeleton className="h-4 w-40" />
										<Skeleton className="mt-2 h-3 w-16" />
									</TableCell>
									<TableCell className={CELL}>
										<Skeleton className="h-5 w-20" />
									</TableCell>
									<TableCell className={cn(CELL, SHOW_RUNS)}>
										<Skeleton className="h-4 w-24" />
									</TableCell>
									<TableCell className={cn(CELL, SHOW_LAST)}>
										<Skeleton className="h-4 w-24" />
									</TableCell>
									<TableCell className={cn(CELL, SHOW_SIZE)}>
										<Skeleton className="h-4 w-16" />
									</TableCell>
									<TableCell className={cn(CELL, SHOW_WIDE)}>
										<Skeleton className="h-4 w-16" />
									</TableCell>
									<TableCell className={cn(CELL, SHOW_WIDE)}>
										<Skeleton className="h-5 w-20" />
									</TableCell>
									<TableCell className={CELL}>
										<Skeleton className="size-10" />
									</TableCell>
								</TableRow>
							))}
						{hosts !== undefined && rows.length === 0 && (
							<TableRow>
								<TableCell colSpan={8} className="h-24 px-4">
									<div className="flex flex-wrap items-center justify-center gap-4">
										<span>{text.filterEmpty}</span>
										<Button variant="outline" size="sm" onClick={clear}>
											{text.clearFilter}
										</Button>
									</div>
								</TableCell>
							</TableRow>
						)}
						{visible.map((host, i) => {
							const group =
								host.state === "healthy" ? "protected" : "attention";
							const first =
								i === 0 ||
								(visible[i - 1].state === "healthy"
									? "protected"
									: "attention") !== group;
							return (
								<Fragment key={host.id}>
									{first && (
										<TableRow className="h-10 bg-muted/50 hover:bg-muted/50">
											<TableHead
												colSpan={8}
												scope="colgroup"
												className={cn(
													CELL,
													"h-10 text-sm font-semibold text-muted-foreground",
												)}
											>
												{text.groups.header(text.groups[group], counts[group])}
											</TableHead>
										</TableRow>
									)}
									<FleetRow
										host={host}
										alerts={alertsByHost.get(host.id) ?? []}
										tab={host.id === activeId ? 0 : -1}
									/>
								</Fragment>
							);
						})}
					</TableBody>
				</Table>
			</div>

			{rows.some((h) => h.recent_runs.length > 0) && (
				<RunLegend className="hidden @min-[1040px]:flex" />
			)}

			{rows.length > PAGE_SIZE && (
				<div className="flex items-center justify-between gap-4">
					<p className="text-sm text-muted-foreground">
						{copy.hosts.showing(
							current * PAGE_SIZE + 1,
							current * PAGE_SIZE + visible.length,
							rows.length,
						)}
					</p>
					<div className="flex gap-2">
						<Button
							variant="outline"
							size="sm"
							disabled={current === 0}
							onClick={() => setPage(current - 1)}
						>
							{copy.hosts.previous}
						</Button>
						<Button
							variant="outline"
							size="sm"
							disabled={current >= lastPage}
							onClick={() => setPage(current + 1)}
						>
							{copy.hosts.next}
						</Button>
					</div>
				</div>
			)}
		</section>
	);
}
