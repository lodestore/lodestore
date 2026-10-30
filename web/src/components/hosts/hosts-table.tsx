import { Link, useNavigate } from "@tanstack/react-router";
import {
	CircleArrowUp,
	MoreHorizontal,
	Search,
	Server,
	TriangleAlert,
} from "lucide-react";
import { useState } from "react";
import type { AgentUpdateStatus, Host, HostUpdate } from "@/api/gen/model";
import {
	Illustration,
	type IllustrationName,
} from "@/components/brand/illustration";
import { AgentVersionCell } from "@/components/hosts/agent-update/agent-version-cell";
import { TextWithPaths } from "@/components/path-text";
import { formatRelative } from "@/components/relative-time";
import { SortButton } from "@/components/sort-button";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
	EmptyTitle,
} from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { Toggle } from "@/components/ui/toggle";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { expiresInDays } from "@/lib/plural";
import { cn } from "@/lib/utils";

const PAGE_SIZE = 50;
const DAY_MS = 86_400_000;
const CERT_WARNING_DAYS = 14;
const SHOW_STATUS = "hidden @min-[680px]:table-cell";
const SHOW_PLATFORM = "hidden @min-[840px]:table-cell";
const SHOW_LAST_SEEN = "hidden @min-[1040px]:table-cell";
const SHOW_WIDE = "hidden @min-[1280px]:table-cell";
const STATUS_RANK = { online: 0, offline: 1, revoked: 2 } as const;

type SortKey = "host" | "status" | "lastSeen";
type Sort = { key: SortKey; dir: "asc" | "desc" };
type Filter = "active" | "revoked";

const byName = (a: Host, b: Host) =>
	a.hostname.localeCompare(b.hostname, undefined, {
		sensitivity: "base",
		numeric: true,
	});

function sortHosts(hosts: Host[], { key, dir }: Sort) {
	const sign = dir === "asc" ? 1 : -1;
	const primary = (a: Host, b: Host) => {
		if (key === "host") return byName(a, b);
		if (key === "status") return STATUS_RANK[a.status] - STATUS_RANK[b.status];
		return (a.last_seen_at ?? -1) - (b.last_seen_at ?? -1);
	};
	return [...hosts].sort(
		(a, b) => sign * primary(a, b) || (key === "host" ? 0 : byName(a, b)),
	);
}

function matches(host: Host, query: string) {
	if (query === "") return true;
	return (
		host.hostname.toLowerCase().includes(query) ||
		host.labels.some((l) => `${l.key}=${l.value}`.toLowerCase().includes(query))
	);
}

export function Truncated({
	text,
	className,
	children,
}: {
	text: string;
	className?: string;
	children?: React.ReactNode;
}) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span className={cn("block truncate", className)}>
					{children ?? text}
				</span>
			</TooltipTrigger>
			<TooltipContent>
				<TextWithPaths text={text} />
			</TooltipContent>
		</Tooltip>
	);
}

function LastSeen({ at }: { at: number }) {
	const date = new Date(at);
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<time
					dateTime={date.toISOString()}
					className="text-xs text-muted-foreground"
				>
					{formatRelative(date, new Date())}
				</time>
			</TooltipTrigger>
			<TooltipContent>
				{date.toLocaleString("en", {
					dateStyle: "medium",
					timeStyle: "medium",
				})}
			</TooltipContent>
		</Tooltip>
	);
}

const labelText = (l: Host["labels"][number]) => `${l.key}=${l.value}`;

const restId = (host: Host) => `labels-rest-${host.id}`;

function LabelChips({
	host,
	onPick,
}: {
	host: Host;
	onPick: (text: string) => void;
}) {
	const { labels } = host;
	if (labels.length === 0) return <span>{copy.common.dash}</span>;
	const rest = labels.slice(2);
	return (
		<div className="flex flex-wrap items-center gap-1">
			{labels.slice(0, 2).map((label) => (
				<Tooltip key={label.key}>
					<TooltipTrigger asChild>
						<Badge
							asChild
							variant="outline"
							className="h-6 max-w-24 min-w-0 cursor-pointer font-mono hover:bg-muted xl:max-w-32"
						>
							<button type="button" onClick={() => onPick(labelText(label))}>
								<span className="truncate">{labelText(label)}</span>
							</button>
						</Badge>
					</TooltipTrigger>
					<TooltipContent>{labelText(label)}</TooltipContent>
				</Tooltip>
			))}
			{rest.length > 0 && (
				<>
					<Tooltip>
						<TooltipTrigger asChild>
							<Badge variant="secondary">+{rest.length}</Badge>
						</TooltipTrigger>
						<TooltipContent>
							<ul className="font-mono">
								{rest.map((label) => (
									<li key={label.key}>{labelText(label)}</li>
								))}
							</ul>
						</TooltipContent>
					</Tooltip>
					<span id={restId(host)} className="sr-only">
						{rest.map(labelText).join(", ")}
					</span>
				</>
			)}
		</div>
	);
}

export function CertificateText({ host }: { host: Host }) {
	if (host.status === "revoked") return <span>{copy.common.dash}</span>;
	const remaining = host.cert_not_after - Date.now();
	if (remaining <= 0) {
		return <span className="text-destructive">{copy.hosts.expired}</span>;
	}
	const days = Math.ceil(remaining / DAY_MS);
	const warn = host.status === "online" && days < CERT_WARNING_DAYS;
	return (
		<span
			className={cn("inline-flex items-center gap-1", warn && "text-warning")}
		>
			{warn && <TriangleAlert aria-hidden className="size-4" />}
			{expiresInDays(days)}
		</span>
	);
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-16">
			<TableCell className="px-4">
				<Skeleton className="h-4 w-40" />
				<Skeleton className="mt-2 h-3 w-16" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_STATUS)}>
				<Skeleton className="h-5 w-20" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_LAST_SEEN)}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_PLATFORM)}>
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-5 w-32" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-12" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_WIDE)}>
				<Skeleton className="h-4 w-28" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="size-10" />
			</TableCell>
		</TableRow>
	));
}

export function HostsTable({
	hosts,
	isPending,
	isError,
	onRetry,
	updates,
	status,
	selectable = false,
	selected,
	onSelect,
	onSelectAll,
	onAdd,
	onEditLabels,
	onRevoke,
}: {
	hosts?: Host[];
	isPending: boolean;
	isError: boolean;
	onRetry: () => void;
	updates?: Map<string, HostUpdate>;
	status?: AgentUpdateStatus;
	selectable?: boolean;
	selected?: Set<string>;
	onSelect?: (id: string, checked: boolean) => void;
	onSelectAll?: (ids: string[], checked: boolean) => void;
	onAdd: () => void;
	onEditLabels: (host: Host) => void;
	onRevoke: (host: Host) => void;
}) {
	const navigate = useNavigate();
	const openHost = (host: Host) =>
		navigate({ to: "/hosts/$id", params: { id: host.id } });
	const [query, setQuery] = useState("");
	const [filter, setFilter] = useState<Filter>("active");
	const [sort, setSort] = useState<Sort>({ key: "status", dir: "asc" });
	const [page, setPage] = useState(0);

	const [onlyOutdated, setOnlyOutdated] = useState(false);

	const needle = query.trim().toLowerCase();
	const isOutdated = (h: Host) => updates?.get(h.id)?.outdated === true;
	const canSelect = (h: Host) =>
		selectable && isOutdated(h) && updates?.get(h.id)?.updatable === true;
	const active = (hosts ?? []).filter((h) => h.status !== "revoked");
	const outdatedCount = selectable ? active.filter(isOutdated).length : 0;
	const outdatedOnly = onlyOutdated && outdatedCount > 0;
	const showSelect = active.some(canSelect);
	const inFilter = (hosts ?? []).filter((h) =>
		filter === "revoked"
			? h.status === "revoked"
			: h.status !== "revoked" && (!outdatedOnly || isOutdated(h)),
	);
	const rows = sortHosts(
		inFilter.filter((h) => matches(h, needle)),
		sort,
	);
	const selectableIds = rows.filter(canSelect).map((h) => h.id);
	const pickedCount = selectableIds.filter((id) => selected?.has(id)).length;
	const allPicked =
		selectableIds.length > 0 && pickedCount === selectableIds.length;
	const lastPage = Math.max(0, Math.ceil(rows.length / PAGE_SIZE) - 1);
	const current = Math.min(page, lastPage);
	const visible = rows.slice(current * PAGE_SIZE, (current + 1) * PAGE_SIZE);

	const sortBy = (key: SortKey) => {
		setPage(0);
		setSort((prev) =>
			prev.key === key
				? { key, dir: prev.dir === "asc" ? "desc" : "asc" }
				: { key, dir: key === "lastSeen" ? "desc" : "asc" },
		);
	};
	const ariaSort = (key: SortKey) =>
		sort.key === key
			? sort.dir === "asc"
				? "ascending"
				: "descending"
			: undefined;

	let body: React.ReactNode = null;
	if (hosts === undefined) {
		if (!isPending) body = false;
	} else if (inFilter.length === 0) {
		body =
			filter === "revoked" ? (
				<EmptyState
					heading={copy.hosts.revokedEmptyHeading}
					body={copy.hosts.revokedEmptyBody}
				/>
			) : (
				<EmptyState
					heading={copy.hosts.emptyHeading}
					body={copy.hosts.emptyBody}
					illustration="hosts-empty"
					action={<Button onClick={onAdd}>{copy.hosts.addHost}</Button>}
				/>
			);
	} else if (rows.length === 0) {
		body = (
			<EmptyState
				heading={copy.hosts.searchEmptyHeading(query.trim())}
				body={copy.hosts.searchEmptyBody}
			/>
		);
	}

	return (
		<div className="flex flex-col gap-4">
			<div className="flex flex-wrap items-center justify-between gap-4">
				<div className="relative w-90 max-w-full">
					<Search
						aria-hidden
						className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
					/>
					<Input
						type="search"
						aria-label={copy.hosts.searchPlaceholder}
						placeholder={copy.hosts.searchPlaceholder}
						value={query}
						className="pl-10"
						onChange={(e) => {
							setQuery(e.target.value);
							setPage(0);
						}}
					/>
				</div>
				<div className="flex flex-wrap items-center gap-2">
					{outdatedCount > 0 && filter === "active" && (
						<Toggle
							variant="outline"
							size="lg"
							pressed={outdatedOnly}
							onPressedChange={(on) => {
								setOnlyOutdated(on);
								setPage(0);
							}}
							className="data-[state=on]:border-primary-strong data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong aria-pressed:bg-primary-subtle"
						>
							<CircleArrowUp aria-hidden />
							{copy.agentUpdate.filter(outdatedCount)}
						</Toggle>
					)}
					<ToggleGroup
						type="single"
						variant="outline"
						size="lg"
						spacing={0}
						value={filter}
						onValueChange={(value) => {
							if (value === "active" || value === "revoked") {
								setFilter(value);
								setPage(0);
							}
						}}
					>
						<ToggleGroupItem
							value="active"
							className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
						>
							{copy.hosts.filterActive}
						</ToggleGroupItem>
						<ToggleGroupItem
							value="revoked"
							className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
						>
							{copy.hosts.filterRevoked}
						</ToggleGroupItem>
					</ToggleGroup>
				</div>
			</div>

			{isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{copy.errors.list("hosts")}{" "}
						<Button variant="link" className="h-auto p-0" onClick={onRetry}>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}

			{body === null ? (
				<div className="@container overflow-hidden rounded-lg border bg-card">
					<Table>
						<TableHeader className="bg-muted">
							<TableRow>
								{showSelect && (
									<TableHead className="w-10 px-4">
										<Checkbox
											aria-label={copy.agentUpdate.selectAll}
											checked={
												allPicked
													? true
													: pickedCount > 0
														? "indeterminate"
														: false
											}
											onCheckedChange={(c) =>
												onSelectAll?.(selectableIds, c === true)
											}
										/>
									</TableHead>
								)}
								<TableHead className="px-4" aria-sort={ariaSort("host")}>
									<SortButton
										label={copy.hosts.columns.host}
										active={sort.key === "host"}
										dir={sort.dir}
										onClick={() => sortBy("host")}
									/>
								</TableHead>
								<TableHead
									className={cn("px-4", SHOW_STATUS)}
									aria-sort={ariaSort("status")}
								>
									<SortButton
										label={copy.hosts.columns.status}
										active={sort.key === "status"}
										dir={sort.dir}
										onClick={() => sortBy("status")}
									/>
								</TableHead>
								<TableHead
									className={cn("px-4", SHOW_LAST_SEEN)}
									aria-sort={ariaSort("lastSeen")}
								>
									<SortButton
										label={copy.hosts.columns.lastSeen}
										active={sort.key === "lastSeen"}
										dir={sort.dir}
										onClick={() => sortBy("lastSeen")}
									/>
								</TableHead>
								<TableHead className={cn("px-4", SHOW_PLATFORM)}>
									{copy.hosts.columns.platform}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_STATUS)}>
									{copy.hosts.columns.labels}
								</TableHead>
								<TableHead className="px-4">
									{copy.hosts.columns.version}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_WIDE)}>
									{copy.hosts.columns.certificate}
								</TableHead>
								<TableHead className="w-16 px-4">
									<span className="sr-only">{copy.hosts.columns.actions}</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{hosts === undefined ? (
								<SkeletonRows />
							) : (
								visible.map((host) => (
									<TableRow
										key={host.id}
										className="h-16 cursor-pointer"
										onClick={(e) => {
											const target = e.target as HTMLElement;
											if (!target.closest("a,button,[role=menuitem]")) {
												void openHost(host);
											}
										}}
									>
										{showSelect && (
											<TableCell
												className="px-4"
												onClick={(e) => e.stopPropagation()}
											>
												{canSelect(host) && (
													<Checkbox
														aria-label={copy.agentUpdate.selectHost(
															host.hostname,
														)}
														checked={selected?.has(host.id) === true}
														onCheckedChange={(c) =>
															onSelect?.(host.id, c === true)
														}
													/>
												)}
											</TableCell>
										)}
										<TableCell className="px-4">
											<Truncated
												text={host.hostname}
												className="max-w-32 @min-[680px]:max-w-48 lg:max-w-32 xl:max-w-52"
											>
												<Link
													to="/hosts/$id"
													params={{ id: host.id }}
													data-host-name={host.id}
													aria-label={copy.hosts.openRow(host.hostname)}
													aria-describedby={
														host.labels.length > 2 ? restId(host) : undefined
													}
													className="block max-w-32 truncate rounded-sm text-left text-base font-semibold @min-[680px]:max-w-48 lg:max-w-32 xl:max-w-52"
												>
													{host.hostname}
												</Link>
											</Truncated>
											<div
												title={host.id}
												className="hidden font-mono text-sm text-muted-foreground lg:block"
											>
												{host.id.slice(-8)}
											</div>
											<div className="@min-[680px]:hidden">
												<StatusBadge status={host.status} />
											</div>
											{host.labels.length > 0 && (
												<div className="max-w-32 font-mono text-sm whitespace-normal wrap-anywhere text-muted-foreground @min-[680px]:hidden">
													{host.labels.map(labelText).join(" ")}
												</div>
											)}
										</TableCell>
										<TableCell className={cn("px-4", SHOW_STATUS)}>
											<StatusBadge status={host.status} />
										</TableCell>
										<TableCell className={cn("px-4", SHOW_LAST_SEEN)}>
											{host.last_seen_at != null ? (
												<LastSeen at={host.last_seen_at} />
											) : (
												copy.common.dash
											)}
										</TableCell>
										<TableCell className={cn("px-4", SHOW_PLATFORM)}>
											<div>
												{host.os} / {host.arch}
											</div>
											{host.distro && (
												<div className="text-sm text-muted-foreground">
													{host.distro}
												</div>
											)}
										</TableCell>
										<TableCell className={cn("px-4", SHOW_STATUS)}>
											<LabelChips
												host={host}
												onPick={(text) => {
													setQuery(text);
													setPage(0);
												}}
											/>
										</TableCell>
										<TableCell className="px-4">
											<AgentVersionCell
												version={host.version}
												update={updates?.get(host.id)}
												status={status}
											/>
										</TableCell>
										<TableCell className={cn("px-4", SHOW_WIDE)}>
											<CertificateText host={host} />
										</TableCell>
										<TableCell
											className="px-4"
											onClick={(e) => e.stopPropagation()}
										>
											<DropdownMenu modal={false}>
												<DropdownMenuTrigger asChild>
													<Button
														variant="ghost"
														size="icon"
														data-host-actions={host.id}
														aria-label={copy.hosts.actionsFor(host.hostname)}
													>
														<MoreHorizontal aria-hidden />
													</Button>
												</DropdownMenuTrigger>
												<DropdownMenuContent align="end">
													<DropdownMenuItem
														onSelect={() => void openHost(host)}
													>
														{copy.hosts.viewDetails}
													</DropdownMenuItem>
													{host.status !== "revoked" && (
														<>
															<DropdownMenuItem
																onSelect={() => onEditLabels(host)}
															>
																{copy.hosts.editLabels}
															</DropdownMenuItem>
															<DropdownMenuSeparator />
															<DropdownMenuItem
																variant="destructive"
																onSelect={() => onRevoke(host)}
															>
																{copy.hosts.revokeHost}
															</DropdownMenuItem>
														</>
													)}
												</DropdownMenuContent>
											</DropdownMenu>
										</TableCell>
									</TableRow>
								))
							)}
						</TableBody>
					</Table>
				</div>
			) : (
				body
			)}

			{hosts !== undefined && rows.length > 0 && (
				<div className="flex items-center justify-between">
					<p className="text-xs text-muted-foreground">
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
		</div>
	);
}

function EmptyState({
	heading,
	body,
	action,
	illustration,
}: {
	heading: string;
	body: string;
	action?: React.ReactNode;
	illustration?: IllustrationName;
}) {
	return (
		<Empty className={illustration ? "py-24" : undefined}>
			<EmptyHeader>
				{illustration ? (
					<Illustration
						name={illustration}
						width={320}
						height={216}
						className="h-auto w-80 max-w-full"
					/>
				) : (
					<EmptyMedia variant="icon">
						<Server />
					</EmptyMedia>
				)}
				<EmptyTitle>{heading}</EmptyTitle>
				<EmptyDescription>{body}</EmptyDescription>
			</EmptyHeader>
			{action && <EmptyContent>{action}</EmptyContent>}
		</Empty>
	);
}
