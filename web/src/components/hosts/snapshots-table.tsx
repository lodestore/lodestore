import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { FolderOpen, RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import {
	getListHostSnapshotsQueryKey,
	useListHostSnapshots,
	useSyncCatalog,
} from "@/api/gen/endpoints";
import type { HostDetail, SnapshotItem, SourceCount } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { SnapshotActionsMenu } from "@/components/snapshots/snapshot-actions-menu";
import {
	type SnapshotAction,
	SnapshotDialogs,
} from "@/components/snapshots/snapshot-dialogs";
import {
	PolicyDeletedBadge,
	ProvenanceBadge,
	SuspiciousBadge,
} from "@/components/snapshots/suspicious-badge";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
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
import { formatBytes, formatCount } from "@/lib/format";
import {
	parseSourceType,
	SOURCE_TYPES,
	type SourceType,
	sourceTypeIcon,
	sourceTypeLabel,
} from "@/lib/source-type";
import { cn } from "@/lib/utils";

const route = getRouteApi("/_app/hosts/$id");

const SHORT_ID = 8;
const ALL = "all";
const SOURCE_CAP = "max-w-36 @min-[1040px]:max-w-48";
const SOURCE_CAP_MIXED =
	"max-w-28 @min-[680px]:max-w-36 @min-[1040px]:max-w-48";
const REFRESH_MS = 15_000;
const FROM_680 = "hidden @min-[680px]:table-cell";
const FROM_840 = "hidden @min-[840px]:table-cell";
const FROM_920 = "hidden @min-[920px]:table-cell";
const FROM_1040 = "hidden @min-[1040px]:table-cell";
const SOURCE_FLEX =
	"w-full max-w-0 @min-[680px]:w-auto @min-[680px]:max-w-none";
const PAD = "px-2 @min-[680px]:px-4";
const PAD_STATUS = "px-1 @min-[680px]:px-4";
const STATUS_ICON =
	"[&_[data-status]>span]:sr-only @min-[680px]:[&_[data-status]>span]:not-sr-only";

const source = (snapshot: SnapshotItem) =>
	snapshot.policy_name ??
	(snapshot.stdin_name
		? copy.snapshots.cliSource(snapshot.stdin_name)
		: copy.jobs.cliBadge);

const contents = (snapshot: SnapshotItem) =>
	snapshot.stdin_name ?? snapshot.paths.join(", ");

const fileTotal = ({ stats }: SnapshotItem) =>
	stats.files_new + stats.files_changed + stats.files_unchanged;

function Truncated({ text, className }: { text: string; className?: string }) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span className={cn("block truncate", className)}>{text}</span>
			</TooltipTrigger>
			<TooltipContent className="max-w-lg wrap-anywhere">
				<TextWithPaths text={text} />
			</TooltipContent>
		</Tooltip>
	);
}

function SourceTypeFilter({
	counts,
	value,
	onChange,
}: {
	counts: SourceCount[];
	value: SourceType | undefined;
	onChange: (next: SourceType | undefined) => void;
}) {
	const total = counts.reduce((sum, { count }) => sum + count, 0);
	const items = SOURCE_TYPES.flatMap((type) => {
		const count = counts.find((c) => c.source_type === type)?.count;
		return count === undefined ? [] : [{ type, count }];
	});
	const options: { type: SourceType | typeof ALL; count: number }[] = [
		{ type: ALL, count: total },
		...items,
	];
	const current = value ?? ALL;
	const change = (next: string) =>
		onChange(next === ALL ? undefined : parseSourceType(next));
	const active = items.find((item) => item.type === value);
	const summary = copy.snapshots.filter.selectValue(
		active ? sourceTypeLabel(active.type) : copy.snapshots.filter.all,
		active?.count ?? total,
	);

	return (
		<>
			<ToggleGroup
				type="single"
				variant="outline"
				size="lg"
				spacing={0}
				aria-label={copy.snapshots.filter.label}
				className="hidden @min-[680px]:flex"
				value={current}
				onValueChange={(next) => next && change(next)}
			>
				{options.map(({ type, count }) => {
					const Icon = type === ALL ? undefined : sourceTypeIcon[type];
					return (
						<ToggleGroupItem
							key={type}
							value={type}
							className="gap-2 px-4 font-normal data-[state=on]:bg-primary-subtle data-[state=on]:font-semibold data-[state=on]:text-primary-strong"
						>
							{Icon && (
								<Icon
									aria-hidden
									className="hidden text-muted-foreground @min-[840px]:block"
								/>
							)}
							{type === ALL ? copy.snapshots.filter.all : sourceTypeLabel(type)}
							<span className="font-normal text-muted-foreground tabular-nums group-data-[state=on]/toggle:font-semibold group-data-[state=on]/toggle:text-primary-strong">
								{count}
							</span>
						</ToggleGroupItem>
					);
				})}
			</ToggleGroup>
			<Select value={current} onValueChange={change}>
				<SelectTrigger
					aria-label={summary}
					className="w-full data-[size=default]:h-10 @min-[680px]:hidden"
				>
					<SelectValue>{summary}</SelectValue>
				</SelectTrigger>
				<SelectContent>
					<SelectItem value={ALL}>
						{copy.snapshots.filter.all}
						<span className="text-muted-foreground tabular-nums">{total}</span>
					</SelectItem>
					{items.map(({ type, count }) => (
						<SelectItem key={type} value={type}>
							{sourceTypeLabel(type)}
							<span className="text-muted-foreground tabular-nums">
								{count}
							</span>
						</SelectItem>
					))}
				</SelectContent>
			</Select>
		</>
	);
}

function TypeIcon({ type }: { type: string }) {
	const Icon = sourceTypeIcon[type as SourceType] ?? sourceTypeIcon.files;
	return <Icon aria-hidden className="size-4 shrink-0 text-muted-foreground" />;
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-12">
			<TableCell className={PAD}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680)}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={cn(PAD, SOURCE_FLEX)}>
				<Skeleton className="h-4 w-24 max-w-full" />
			</TableCell>
			<TableCell className={cn("px-4", FROM_920)}>
				<Skeleton className="h-4 w-40" />
			</TableCell>
			<TableCell className={cn("px-4", FROM_840)}>
				<Skeleton className="h-4 w-14" />
			</TableCell>
			<TableCell className={cn("px-4", FROM_1040)}>
				<Skeleton className="h-4 w-12" />
			</TableCell>
			<TableCell className={PAD_STATUS}>
				<Skeleton className="h-4 w-6 @min-[680px]:w-16" />
			</TableCell>
			<TableCell className={PAD}>
				<Skeleton className="h-8 w-36" />
			</TableCell>
		</TableRow>
	));
}

export function SnapshotsTable({ host }: { host: HostDetail }) {
	const queryClient = useQueryClient();
	const [page, setPage] = useState(1);
	const type = parseSourceType(route.useSearch().type);
	const navigate = route.useNavigate();
	const [action, setAction] = useState<{
		snapshot: SnapshotItem;
		action: SnapshotAction;
	}>();
	const query = useListHostSnapshots(
		host.id,
		{ page, source_type: type },
		{
			query: {
				placeholderData: keepPreviousData,
				refetchInterval: REFRESH_MS,
				refetchIntervalInBackground: false,
			},
		},
	);
	const sync = useSyncCatalog();
	const data = query.data;
	const first = data ? (data.page - 1) * data.page_size : 0;
	const lastPage = data
		? Math.max(1, Math.ceil(data.total / data.page_size))
		: 1;

	const counts = data?.source_counts ?? [];
	const several = counts.length > 1;
	const showFiles = several ? FROM_1040 : FROM_920;

	function selectType(next: SourceType | undefined) {
		setPage(1);
		void navigate({ search: (s) => ({ ...s, type: next }), replace: true });
	}

	useEffect(() => {
		if (data && !query.isPlaceholderData && page > lastPage) {
			setPage(lastPage);
		}
	}, [data, query.isPlaceholderData, page, lastPage]);

	useEffect(() => {
		if (
			data &&
			type &&
			!query.isPlaceholderData &&
			!data.source_counts.some((c) => c.source_type === type)
		) {
			void navigate({
				search: (s) => ({ ...s, type: undefined }),
				replace: true,
			});
		}
	}, [data, type, query.isPlaceholderData, navigate]);

	function checkStorage() {
		sync.mutate(
			{ id: host.repository_id },
			{
				onSuccess: ({ added }) => {
					toast.success(copy.snapshots.catalogUpdated(added));
					void queryClient.invalidateQueries({
						queryKey: getListHostSnapshotsQueryKey(host.id),
					});
				},
				onError: (error) =>
					toast.error(
						copy.errors.submit(copy.snapshots.syncFailed, error.message),
					),
			},
		);
	}

	return (
		<div className="flex flex-col gap-4 pt-4">
			<div className="@container">
				<div
					className={cn(
						"flex",
						several
							? "flex-col gap-4 @min-[680px]:flex-row @min-[680px]:flex-wrap @min-[680px]:items-center @min-[680px]:justify-between"
							: "justify-end",
					)}
				>
					{several && (
						<SourceTypeFilter
							counts={counts}
							value={type}
							onChange={selectType}
						/>
					)}
					<Button
						variant="outline"
						size="sm"
						className={cn(
							several &&
								"self-start @min-[680px]:ml-auto @min-[680px]:self-center",
						)}
						disabled={sync.isPending}
						onClick={checkStorage}
					>
						{sync.isPending ? <Spinner /> : <RefreshCw aria-hidden />}
						{sync.isPending
							? copy.snapshots.syncing
							: copy.snapshots.syncCatalog}
					</Button>
				</div>
			</div>

			{data === undefined && query.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.snapshots.loadFailed}
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => query.refetch()}
						>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}

			{data?.total === 0 && (
				<Empty className="py-24">
					<EmptyHeader>
						<Illustration
							name="preview-host-tabs"
							width={320}
							height={216}
							className="h-auto w-80 max-w-full"
						/>
						<EmptyTitle>{copy.snapshots.emptyHeading}</EmptyTitle>
						<EmptyDescription>{copy.snapshots.emptyBody}</EmptyDescription>
					</EmptyHeader>
				</Empty>
			)}

			{(query.isPending || (data !== undefined && data.total > 0)) && (
				<div className="@container overflow-hidden rounded-lg border bg-card">
					<Table>
						<TableHeader className="bg-muted">
							<TableRow>
								<TableHead className={PAD}>
									{copy.snapshots.columns.created}
								</TableHead>
								<TableHead className={cn(PAD, FROM_680)}>
									{copy.snapshots.columns.snapshot}
								</TableHead>
								<TableHead className={cn(PAD, SOURCE_FLEX)}>
									{copy.snapshots.columns.source}
								</TableHead>
								<TableHead className={cn("px-4", FROM_920)}>
									{copy.snapshots.columns.contents}
								</TableHead>
								<TableHead className={cn("px-4 text-right", FROM_840)}>
									{copy.snapshots.columns.size}
								</TableHead>
								<TableHead className={cn("px-4 text-right", showFiles)}>
									{copy.snapshots.columns.files}
								</TableHead>
								<TableHead className={PAD_STATUS}>
									<span className="sr-only @min-[680px]:not-sr-only">
										{copy.snapshots.columns.status}
									</span>
								</TableHead>
								<TableHead className={PAD}>
									<span className="sr-only">
										{copy.snapshots.columns.actions}
									</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{data === undefined ? (
								<SkeletonRows />
							) : (
								data.items.map((snapshot) => (
									<TableRow key={snapshot.id} className="h-12">
										<TableCell className={cn("text-sm", PAD)}>
											<RelativeTime value={new Date(snapshot.time)} />
										</TableCell>
										<TableCell
											className={cn("font-mono text-sm", PAD, FROM_680)}
											title={snapshot.id}
										>
											{snapshot.id.slice(0, SHORT_ID)}
										</TableCell>
										<TableCell
											className={cn("text-sm font-semibold", PAD, SOURCE_FLEX)}
										>
											<div
												className={cn(
													"flex flex-col items-start py-1",
													several ? cn("gap-0", SOURCE_CAP_MIXED) : "gap-1",
												)}
											>
												{several && (
													<span className="flex max-w-full items-center gap-2">
														<TypeIcon type={snapshot.source_type} />
														<Truncated
															text={
																snapshot.source_label ??
																sourceTypeLabel(snapshot.source_type)
															}
															className="min-w-0"
														/>
													</span>
												)}
												<Truncated
													text={source(snapshot)}
													className={
														several
															? "max-w-full font-normal text-muted-foreground"
															: SOURCE_CAP
													}
												/>
												{snapshot.policy_deleted && <PolicyDeletedBadge />}
											</div>
										</TableCell>
										<TableCell
											className={cn("w-full max-w-0 px-4 text-sm", FROM_920)}
										>
											<Truncated
												text={contents(snapshot)}
												className="font-mono"
											/>
										</TableCell>
										<TableCell
											className={cn(
												"px-4 text-right text-sm tabular-nums",
												FROM_840,
											)}
										>
											{formatBytes(snapshot.stats.bytes_total)}
										</TableCell>
										<TableCell
											className={cn(
												"px-4 text-right text-sm tabular-nums",
												showFiles,
											)}
										>
											{formatCount(fileTotal(snapshot))}
										</TableCell>
										<TableCell className={PAD_STATUS}>
											<div
												className={cn(
													"flex flex-wrap items-center gap-1",
													STATUS_ICON,
												)}
											>
												<ProvenanceBadge snapshot={snapshot} />
												{snapshot.suspicious && (
													<SuspiciousBadge snapshot={snapshot} />
												)}
												{snapshot.incomplete ? (
													<StatusBadge status="warning">
														{copy.snapshots.incomplete}
													</StatusBadge>
												) : (
													<StatusBadge status="success">
														{copy.snapshots.complete}
													</StatusBadge>
												)}
											</div>
										</TableCell>
										<TableCell className={cn("@min-[840px]:min-w-48", PAD)}>
											<div className="flex items-center justify-end gap-1">
												<Button asChild variant="outline" size="sm">
													<Link
														to="/restore"
														search={{ host: host.id, snapshot: snapshot.id }}
														aria-label={copy.snapshots.browseFilesFor(
															snapshot.id.slice(0, SHORT_ID),
														)}
													>
														<FolderOpen
															aria-hidden
															className="@min-[840px]:hidden"
														/>
														<span className="sr-only @min-[840px]:not-sr-only">
															{copy.snapshots.browseFiles}
														</span>
													</Link>
												</Button>
												<SnapshotActionsMenu
													snapshot={snapshot}
													onAction={(next) =>
														setAction({ snapshot, action: next })
													}
												/>
											</div>
										</TableCell>
									</TableRow>
								))
							)}
						</TableBody>
					</Table>
				</div>
			)}

			<SnapshotDialogs target={action} onClose={() => setAction(undefined)} />

			{data !== undefined && data.total > 0 && (
				<>
					<div className="flex items-center justify-between">
						<p className="text-xs text-muted-foreground">
							{copy.jobs.showing(
								first + 1,
								first + data.items.length,
								data.total,
							)}
						</p>
						<div className="flex gap-2">
							<Button
								variant="outline"
								size="sm"
								disabled={page <= 1}
								onClick={() => setPage(page - 1)}
							>
								{copy.jobs.previous}
							</Button>
							<Button
								variant="outline"
								size="sm"
								disabled={page >= lastPage}
								onClick={() => setPage(page + 1)}
							>
								{copy.jobs.next}
							</Button>
						</div>
					</div>
					<p className="text-sm text-muted-foreground">
						{copy.snapshots.footnote}
					</p>
				</>
			)}
		</div>
	);
}
