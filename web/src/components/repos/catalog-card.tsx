import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ChevronDown, TriangleAlert } from "lucide-react";
import {
	getListCatalogStatusQueryKey,
	useListCatalogStatus,
	useRebuildCatalog,
} from "@/api/gen/endpoints";
import type { CatalogStatus } from "@/api/gen/model";
import { PathText, TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Progress } from "@/components/ui/progress";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const ACTIVE_POLL_MS = 2_000;
const IDLE_POLL_MS = 15_000;
const SKIPPED_SHOWN = 10;

export const isActivePhase = (phase?: CatalogStatus["phase"]) =>
	phase === "listing" || phase === "reading";

export const isRebuilding = (status: CatalogStatus) =>
	isActivePhase(status.phase);

export function useCatalogStatus() {
	return useListCatalogStatus({
		query: {
			refetchInterval: (query) =>
				query.state.data?.some(isRebuilding) ? ACTIVE_POLL_MS : IDLE_POLL_MS,
			refetchIntervalInBackground: false,
		},
	});
}

function Fact({
	label,
	children,
}: {
	label: string;
	children: React.ReactNode;
}) {
	return (
		<div className="flex flex-col gap-1">
			<dt className="text-sm text-muted-foreground">{label}</dt>
			<dd className="font-semibold">{children}</dd>
		</div>
	);
}

function RebuildProgress({ status }: { status: CatalogStatus }) {
	const listing = status.phase === "listing" || !status.total;
	const total = status.total ?? 0;
	const text = listing
		? copy.catalog.phase.listing
		: copy.catalog.phase.reading;
	return (
		<div className="flex flex-col gap-2">
			<p className="text-sm font-semibold">{text}</p>
			<Progress
				aria-label={text}
				value={listing ? undefined : Math.round((status.read / total) * 100)}
			/>
			{!listing && (
				<p className="text-sm tabular-nums">
					{copy.catalog.progress(status.read, total)}
				</p>
			)}
			<p className="text-sm text-muted-foreground">
				{copy.catalog.appearAsRead}{" "}
				<Link to="/restore" className="text-primary-strong underline">
					{copy.catalog.openRestore}
				</Link>
			</p>
		</div>
	);
}

function SkippedAlert({ status }: { status: CatalogStatus }) {
	const shown = status.skipped.slice(0, SKIPPED_SHOWN);
	const more = status.skipped.length - shown.length;
	return (
		<Alert className="text-warning">
			<TriangleAlert aria-hidden />
			<AlertDescription className="flex flex-col gap-2 text-foreground">
				{copy.catalog.skipped(status.skipped.length)}
				<Collapsible>
					<CollapsibleTrigger className="group flex items-center gap-1 text-sm font-semibold outline-none focus-visible:ring-2 focus-visible:ring-ring">
						{copy.catalog.skippedList}
						<ChevronDown
							aria-hidden
							className="size-4 transition-transform group-data-[state=open]:rotate-180"
						/>
					</CollapsibleTrigger>
					<CollapsibleContent>
						<ul className="mt-2 flex flex-col gap-1">
							{shown.map((item) => (
								<li key={item.key} className="text-sm">
									<span className="font-mono">
										<PathText value={item.key} />
									</span>
									{item.reason && (
										<span className="text-muted-foreground">
											{" "}
											· {item.reason}
										</span>
									)}
								</li>
							))}
							{more > 0 && (
								<li className="text-sm text-muted-foreground">
									{copy.catalog.skippedMore(more)}
								</li>
							)}
						</ul>
					</CollapsibleContent>
				</Collapsible>
			</AlertDescription>
		</Alert>
	);
}

export function CatalogCard({ repositoryId }: { repositoryId: string }) {
	const queryClient = useQueryClient();
	const statuses = useCatalogStatus();
	const status = statuses.data?.find(
		(item) => item.repository_id === repositoryId,
	);
	const rebuild = useRebuildCatalog({
		mutation: {
			onSuccess: (started) => {
				queryClient.setQueryData<CatalogStatus[]>(
					getListCatalogStatusQueryKey(),
					(list = []) =>
						list.some((item) => item.repository_id === repositoryId)
							? list.map((item) =>
									item.repository_id === repositoryId
										? {
												...started,
												repository_name: item.repository_name,
												last_sync_at: item.last_sync_at,
												snapshots: item.snapshots,
											}
										: item,
								)
							: [...list, started],
				);
				void queryClient.invalidateQueries({
					queryKey: getListCatalogStatusQueryKey(),
				});
			},
		},
	});
	const running = rebuild.isPending || (status && isRebuilding(status));
	const failure =
		status?.phase === "failed"
			? copy.catalog.failed(status.error)
			: rebuild.isError
				? copy.catalog.failed(rebuild.error.message)
				: undefined;

	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2>{copy.catalog.title}</h2>
				</CardTitle>
				<CardDescription>{copy.catalog.description}</CardDescription>
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				<dl className="flex flex-wrap gap-x-12 gap-y-4">
					<Fact label={copy.catalog.lastSynced}>
						{status?.last_sync_at ? (
							<RelativeTime value={new Date(status.last_sync_at)} />
						) : status ? (
							copy.catalog.neverSynced
						) : (
							copy.common.dash
						)}
					</Fact>
					<Fact label={copy.catalog.snapshots}>
						<span className="tabular-nums">
							{status?.snapshots ?? copy.common.dash}
						</span>
					</Fact>
				</dl>
				{status && isRebuilding(status) && <RebuildProgress status={status} />}
				{failure && (
					<Alert variant="destructive">
						<AlertDescription>
							<TextWithPaths text={failure} />
						</AlertDescription>
					</Alert>
				)}
				{status?.phase === "done" && status.skipped.length > 0 && (
					<SkippedAlert status={status} />
				)}
			</CardContent>
			<CardFooter>
				<Button
					type="button"
					variant="outline"
					disabled={running}
					onClick={() => rebuild.mutate({ id: repositoryId })}
				>
					{running && <Spinner />}
					{copy.catalog.rebuild}
				</Button>
			</CardFooter>
		</Card>
	);
}
