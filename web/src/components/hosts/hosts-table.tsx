import {
	ArrowDown,
	ArrowUp,
	ChevronsUpDown,
	MoreHorizontal,
	Search,
	Server,
	TriangleAlert,
} from "lucide-react";
import { useState } from "react";
import type { Host } from "@/api/gen/model";
import { formatRelative } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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

function Truncated({
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
			<TooltipContent>{text}</TooltipContent>
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
					tabIndex={0}
					className="text-xs text-muted-foreground"
				>
					{copy.hosts.lastSeen(formatRelative(date, new Date()))}
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

function LabelChips({ labels }: { labels: Host["labels"] }) {
	if (labels.length === 0) return <span>{copy.common.dash}</span>;
	const text = (l: Host["labels"][number]) => `${l.key}=${l.value}`;
	const rest = labels.slice(3);
	return (
		<div className="flex max-w-[420px] flex-wrap gap-1">
			{labels.slice(0, 3).map((label) => (
				<Tooltip key={label.key}>
					<TooltipTrigger asChild>
						<Badge variant="outline" className="max-w-40 min-w-0 font-mono">
							<span className="truncate">{text(label)}</span>
						</Badge>
					</TooltipTrigger>
					<TooltipContent>{text(label)}</TooltipContent>
				</Tooltip>
			))}
			{rest.length > 0 && (
				<Tooltip>
					<TooltipTrigger asChild>
						<Badge variant="secondary" aria-label={rest.map(text).join(", ")}>
							+{rest.length}
						</Badge>
					</TooltipTrigger>
					<TooltipContent>
						<ul className="font-mono">
							{rest.map((label) => (
								<li key={label.key}>{text(label)}</li>
							))}
						</ul>
					</TooltipContent>
				</Tooltip>
			)}
		</div>
	);
}

function VersionCell({
	version,
	serverVersion,
}: {
	version: string;
	serverVersion?: string;
}) {
	const differs = serverVersion !== undefined && version !== serverVersion;
	return (
		<span className="inline-flex items-center gap-1 font-mono">
			{version}
			{differs && (
				<Tooltip>
					<TooltipTrigger asChild>
						<span
							role="img"
							aria-label={copy.hosts.versionDiffers(serverVersion)}
							className="text-warning"
						>
							<TriangleAlert aria-hidden className="size-4" />
						</span>
					</TooltipTrigger>
					<TooltipContent>
						{copy.hosts.versionDiffers(serverVersion)}
					</TooltipContent>
				</Tooltip>
			)}
		</span>
	);
}

function CertificateText({ host }: { host: Host }) {
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

function SortButton({
	label,
	active,
	dir,
	onClick,
}: {
	label: string;
	active: boolean;
	dir: Sort["dir"];
	onClick: () => void;
}) {
	const Icon = !active ? ChevronsUpDown : dir === "asc" ? ArrowUp : ArrowDown;
	return (
		<button
			type="button"
			onClick={onClick}
			className="inline-flex items-center gap-1 rounded-sm hover:text-foreground"
		>
			{label}
			<Icon aria-hidden className="size-3.5" />
		</button>
	);
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden>
			<TableCell>
				<Skeleton className="h-4 w-40" />
				<Skeleton className="mt-1 h-3 w-16" />
			</TableCell>
			<TableCell>
				<Skeleton className="h-5 w-20" />
				<Skeleton className="mt-1 h-3 w-28" />
			</TableCell>
			<TableCell>
				<Skeleton className="h-4 w-12" />
			</TableCell>
			<TableCell>
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell>
				<Skeleton className="h-5 w-32" />
			</TableCell>
			<TableCell>
				<Skeleton className="h-4 w-28" />
			</TableCell>
			<TableCell>
				<Skeleton className="size-8" />
			</TableCell>
		</TableRow>
	));
}

export function HostsTable({
	hosts,
	isPending,
	isError,
	onRetry,
	serverVersion,
	onAdd,
	onView,
	onEditLabels,
	onRevoke,
}: {
	hosts?: Host[];
	isPending: boolean;
	isError: boolean;
	onRetry: () => void;
	serverVersion?: string;
	onAdd: () => void;
	onView: (host: Host, via: "row" | "menu") => void;
	onEditLabels: (host: Host) => void;
	onRevoke: (host: Host) => void;
}) {
	const [query, setQuery] = useState("");
	const [filter, setFilter] = useState<Filter>("active");
	const [sort, setSort] = useState<Sort>({ key: "status", dir: "asc" });
	const [page, setPage] = useState(0);

	const needle = query.trim().toLowerCase();
	const inFilter = (hosts ?? []).filter((h) =>
		filter === "revoked" ? h.status === "revoked" : h.status !== "revoked",
	);
	const rows = sortHosts(
		inFilter.filter((h) => matches(h, needle)),
		sort,
	);
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
	const ariaSort = (...keys: SortKey[]) =>
		keys.includes(sort.key)
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
			<div className="flex items-center justify-between gap-4">
				<div className="relative w-80 max-w-full">
					<Search
						aria-hidden
						className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground"
					/>
					<Input
						type="search"
						aria-label={copy.hosts.searchPlaceholder}
						placeholder={copy.hosts.searchPlaceholder}
						value={query}
						className="pl-8"
						onChange={(e) => {
							setQuery(e.target.value);
							setPage(0);
						}}
					/>
				</div>
				<ToggleGroup
					type="single"
					variant="outline"
					value={filter}
					onValueChange={(value) => {
						if (value === "active" || value === "revoked") {
							setFilter(value);
							setPage(0);
						}
					}}
				>
					<ToggleGroupItem value="active">
						{copy.hosts.filterActive}
					</ToggleGroupItem>
					<ToggleGroupItem value="revoked">
						{copy.hosts.filterRevoked}
					</ToggleGroupItem>
				</ToggleGroup>
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
				<div className="rounded-md border">
					<Table className="min-w-[1000px]">
						<TableHeader>
							<TableRow>
								<TableHead aria-sort={ariaSort("host")}>
									<SortButton
										label={copy.hosts.columns.host}
										active={sort.key === "host"}
										dir={sort.dir}
										onClick={() => sortBy("host")}
									/>
								</TableHead>
								<TableHead aria-sort={ariaSort("status", "lastSeen")}>
									<div className="flex items-center gap-3">
										<SortButton
											label={copy.hosts.columns.status}
											active={sort.key === "status"}
											dir={sort.dir}
											onClick={() => sortBy("status")}
										/>
										<SortButton
											label={copy.hosts.columns.lastSeenSort}
											active={sort.key === "lastSeen"}
											dir={sort.dir}
											onClick={() => sortBy("lastSeen")}
										/>
									</div>
								</TableHead>
								<TableHead>{copy.hosts.columns.version}</TableHead>
								<TableHead>{copy.hosts.columns.platform}</TableHead>
								<TableHead>{copy.hosts.columns.labels}</TableHead>
								<TableHead>{copy.hosts.columns.certificate}</TableHead>
								<TableHead className="w-12">
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
										className="cursor-pointer"
										onClick={() => onView(host, "row")}
									>
										<TableCell>
											<Truncated text={host.hostname} className="max-w-[240px]">
												<button
													type="button"
													data-host-name={host.id}
													className="block max-w-[240px] truncate rounded-sm text-left text-sm font-semibold"
												>
													{host.hostname}
												</button>
											</Truncated>
											<div className="font-mono text-xs text-muted-foreground">
												{host.id.slice(0, 8)}
											</div>
										</TableCell>
										<TableCell>
											<StatusBadge status={host.status} />
											{host.last_seen_at != null && (
												<div>
													<LastSeen at={host.last_seen_at} />
												</div>
											)}
										</TableCell>
										<TableCell>
											<VersionCell
												version={host.version}
												serverVersion={serverVersion}
											/>
										</TableCell>
										<TableCell>
											<div>
												{host.os} / {host.arch}
											</div>
											{host.distro && (
												<div className="text-xs text-muted-foreground">
													{host.distro}
												</div>
											)}
										</TableCell>
										<TableCell>
											<LabelChips labels={host.labels} />
										</TableCell>
										<TableCell>
											<CertificateText host={host} />
										</TableCell>
										<TableCell onClick={(e) => e.stopPropagation()}>
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
														onSelect={() => onView(host, "menu")}
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
}: {
	heading: string;
	body: string;
	action?: React.ReactNode;
}) {
	return (
		<Empty>
			<EmptyHeader>
				<EmptyMedia variant="icon">
					<Server />
				</EmptyMedia>
				<EmptyTitle>{heading}</EmptyTitle>
				<EmptyDescription>{body}</EmptyDescription>
			</EmptyHeader>
			{action && <EmptyContent>{action}</EmptyContent>}
		</Empty>
	);
}
