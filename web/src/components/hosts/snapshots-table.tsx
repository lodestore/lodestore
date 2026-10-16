import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { RefreshCw } from "lucide-react";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import {
	getListHostSnapshotsQueryKey,
	useListHostSnapshots,
	useSyncCatalog,
} from "@/api/gen/endpoints";
import type { HostDetail, SnapshotItem } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
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
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { formatBytes, formatCount } from "@/lib/format";
import { cn } from "@/lib/utils";

const SHORT_ID = 8;
const REFRESH_MS = 15_000;
const SHOW_CONTENTS = "hidden @min-[680px]:table-cell";
const SHOW_FILES = "hidden @min-[840px]:table-cell";

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

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-12">
			<TableCell className="px-4">
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_CONTENTS)}>
				<Skeleton className="h-4 w-40" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-14" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_FILES)}>
				<Skeleton className="h-4 w-12" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-16" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-8 w-28" />
			</TableCell>
		</TableRow>
	));
}

export function SnapshotsTable({ host }: { host: HostDetail }) {
	const queryClient = useQueryClient();
	const [page, setPage] = useState(1);
	const query = useListHostSnapshots(
		host.id,
		{ page },
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

	useEffect(() => {
		if (data && !query.isPlaceholderData && page > lastPage) {
			setPage(lastPage);
		}
	}, [data, query.isPlaceholderData, page, lastPage]);

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
			<div className="flex justify-end">
				<Button
					variant="outline"
					size="sm"
					disabled={sync.isPending}
					onClick={checkStorage}
				>
					{sync.isPending ? <Spinner /> : <RefreshCw aria-hidden />}
					{sync.isPending ? copy.snapshots.syncing : copy.snapshots.syncCatalog}
				</Button>
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
								<TableHead className="px-4">
									{copy.snapshots.columns.created}
								</TableHead>
								<TableHead className="px-4">
									{copy.snapshots.columns.snapshot}
								</TableHead>
								<TableHead className="px-4">
									{copy.snapshots.columns.source}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_CONTENTS)}>
									{copy.snapshots.columns.contents}
								</TableHead>
								<TableHead className="px-4 text-right">
									{copy.snapshots.columns.size}
								</TableHead>
								<TableHead className={cn("px-4 text-right", SHOW_FILES)}>
									{copy.snapshots.columns.files}
								</TableHead>
								<TableHead className="px-4">
									{copy.snapshots.columns.status}
								</TableHead>
								<TableHead className="px-4">
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
										<TableCell className="px-4 text-sm">
											<RelativeTime value={new Date(snapshot.time)} />
										</TableCell>
										<TableCell
											className="px-4 font-mono text-sm"
											title={snapshot.id}
										>
											{snapshot.id.slice(0, SHORT_ID)}
										</TableCell>
										<TableCell className="px-4 text-sm font-semibold">
											<Truncated text={source(snapshot)} className="max-w-48" />
										</TableCell>
										<TableCell className={cn("px-4 text-sm", SHOW_CONTENTS)}>
											<Truncated
												text={contents(snapshot)}
												className="max-w-80 font-mono"
											/>
										</TableCell>
										<TableCell className="px-4 text-right text-sm tabular-nums">
											{formatBytes(snapshot.stats.bytes_total)}
										</TableCell>
										<TableCell
											className={cn(
												"px-4 text-right text-sm tabular-nums",
												SHOW_FILES,
											)}
										>
											{formatCount(fileTotal(snapshot))}
										</TableCell>
										<TableCell className="px-4">
											{snapshot.incomplete ? (
												<StatusBadge status="warning">
													{copy.snapshots.incomplete}
												</StatusBadge>
											) : (
												<StatusBadge status="success">
													{copy.snapshots.complete}
												</StatusBadge>
											)}
										</TableCell>
										<TableCell className="px-4 text-right">
											<Button asChild variant="outline" size="sm">
												<Link
													to="/restore"
													search={{ host: host.id, snapshot: snapshot.id }}
													aria-label={copy.snapshots.browseFilesFor(
														snapshot.id.slice(0, SHORT_ID),
													)}
												>
													{copy.snapshots.browseFiles}
												</Link>
											</Button>
										</TableCell>
									</TableRow>
								))
							)}
						</TableBody>
					</Table>
				</div>
			)}

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
