import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { Info } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useGetSnapshot, useListRestoreHosts } from "@/api/gen/endpoints";
import { Illustration } from "@/components/brand/illustration";
import { FileBrowser } from "@/components/restore/file-browser";
import { HostPicker } from "@/components/restore/host-picker";
import { SnapshotHeader } from "@/components/restore/snapshot-header";
import {
	SnapshotTimeline,
	useSnapshotTimeline,
} from "@/components/restore/snapshot-timeline";
import { useSelection } from "@/components/restore/use-selection";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { useIsWide } from "@/hooks/use-wide";
import { formatShortDateTime } from "@/lib/format";
import { snapshotCount } from "@/lib/plural";
import { repositoriesQuery } from "@/lib/session";
import { shortId } from "@/lib/snapshot";

const route = getRouteApi("/_app/restore");
const REFRESH_MS = 15_000;

function RetryAlert({
	children,
	onRetry,
}: {
	children: React.ReactNode;
	onRetry: () => void;
}) {
	return (
		<Alert variant="destructive" role="alert">
			<AlertDescription className="flex items-center justify-between gap-4">
				{children}
				<Button type="button" variant="secondary" size="sm" onClick={onRetry}>
					{copy.errors.retry}
				</Button>
			</AlertDescription>
		</Alert>
	);
}

function NothingToRestore() {
	const repositories = useQuery(repositoriesQuery);
	const hasRepository = (repositories.data ?? []).length > 0;
	return (
		<Empty className="py-24">
			<EmptyHeader>
				<Illustration
					name="preview-restore"
					width={320}
					height={216}
					className="h-auto w-80 max-w-full"
				/>
				<EmptyTitle>{copy.restore.empty.heading}</EmptyTitle>
				<EmptyDescription>{copy.restore.empty.body}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					{hasRepository || repositories.isPending ? (
						<Link to="/policies/new">{copy.restore.empty.createPolicy}</Link>
					) : (
						<Link to="/settings/repositories/new">
							{copy.restore.empty.addRepository}
						</Link>
					)}
				</Button>
			</EmptyContent>
		</Empty>
	);
}

export function RestoreWorkspace() {
	const { host, snapshot, path, q } = route.useSearch();
	const navigate = route.useNavigate();
	const wide = useIsWide();
	const [pickerOpen, setPickerOpen] = useState(false);

	const hosts = useListRestoreHosts({
		query: { refetchInterval: REFRESH_MS, refetchIntervalInBackground: false },
	});
	const detail = useGetSnapshot(snapshot ?? "", {
		query: { enabled: snapshot !== undefined },
	});

	const newest = (hosts.data ?? [])
		.filter((candidate) => candidate.latest_snapshot_ms != null)
		.sort(
			(a, b) => (b.latest_snapshot_ms ?? 0) - (a.latest_snapshot_ms ?? 0),
		)[0];
	const hostKey =
		host ?? (snapshot === undefined ? newest?.key : detail.data?.host_key);
	const timeline = useSnapshotTimeline(hostKey);
	const snapshotId = snapshot ?? timeline.data?.items[0]?.id;

	const selection = useSelection();
	const clearSelection = selection.clear;
	const scope = `${hostKey}|${snapshotId}`;
	const lastScope = useRef(scope);
	useEffect(() => {
		if (lastScope.current === scope) return;
		lastScope.current = scope;
		clearSelection(copy.restore.selection.clearedSnapshot);
	}, [scope, clearSelection]);

	useEffect(() => {
		if (hostKey === undefined) return;
		if (host === hostKey && (snapshot !== undefined || !snapshotId)) return;
		void navigate({
			search: (prev) => ({ ...prev, host: hostKey, snapshot: snapshotId }),
			replace: true,
		});
	}, [host, hostKey, snapshot, snapshotId, navigate]);

	const select = useCallback(
		(next: { host?: string; snapshot?: string }) =>
			navigate({
				search: (prev) => ({
					...prev,
					host: next.host ?? prev.host,
					snapshot: next.snapshot,
					path: undefined,
					q: undefined,
				}),
			}),
		[navigate],
	);
	const selectSnapshot = useCallback(
		(id: string) => {
			setPickerOpen(false);
			void select({ snapshot: id });
		},
		[select],
	);
	const selectHost = useCallback(
		(key: string) => {
			setPickerOpen(false);
			void select({ host: key, snapshot: undefined });
		},
		[select],
	);

	const gone =
		detail.error instanceof ApiError &&
		detail.error.status === 404 &&
		snapshot !== undefined;

	const waiting =
		host === undefined && snapshot !== undefined && detail.isPending;

	let main: React.ReactNode;
	if (hosts.isError && hosts.data === undefined) {
		main = (
			<RetryAlert onRetry={() => hosts.refetch()}>
				{copy.restore.hostPicker.loadFailed}
			</RetryAlert>
		);
	} else if (hosts.data === undefined || waiting) {
		main = <Skeleton aria-hidden className="h-64 w-full" />;
	} else if (hostKey === undefined && snapshot === undefined) {
		main = <NothingToRestore />;
	} else {
		const hostName = hosts.data.find((h) => h.key === hostKey)?.hostname;
		const empty = timeline.data?.total === 0;
		let detailPane: React.ReactNode;
		if (empty) {
			detailPane = (
				<Empty className="py-24">
					<EmptyHeader>
						<Illustration
							name="preview-host-tabs"
							width={320}
							height={216}
							className="h-auto w-80 max-w-full"
						/>
						<EmptyTitle>
							{copy.restore.timeline.emptyHeading(hostName ?? hostKey ?? "")}
						</EmptyTitle>
						<EmptyDescription>
							{copy.restore.timeline.emptyBody}
						</EmptyDescription>
					</EmptyHeader>
				</Empty>
			);
		} else if (gone) {
			detailPane = (
				<Alert role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						<span>
							<strong className="font-semibold">
								{copy.restore.snapshot.goneHeading}
							</strong>{" "}
							{copy.restore.snapshot.goneBody}
						</span>
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => select({ snapshot: undefined })}
						>
							{copy.restore.snapshot.back}
						</Button>
					</AlertDescription>
				</Alert>
			);
		} else if (detail.isError) {
			detailPane = (
				<RetryAlert onRetry={() => detail.refetch()}>
					{copy.restore.snapshot.openFailed(detail.error.message)}
				</RetryAlert>
			);
		} else {
			detailPane = <SnapshotHeader snapshot={detail.data} />;
		}

		const showBrowser =
			snapshotId !== undefined && !empty && !gone && !detail.isError;
		const currentHost = hosts.data.find((h) => h.key === hostKey);
		const timelineCard = hostKey !== undefined && !empty && (
			<SnapshotTimeline
				hostKey={hostKey}
				selected={snapshotId}
				onSelect={selectSnapshot}
				scrollClassName={
					wide
						? "[&_[data-slot=scroll-area-viewport]]:max-h-[calc(100dvh-14rem)]"
						: "[&_[data-slot=scroll-area-viewport]]:max-h-80"
				}
			/>
		);
		const current =
			timeline.data?.items.find((item) => item.id === snapshotId) ??
			detail.data;
		const detailStack = (
			<div className="flex min-w-0 flex-col gap-6">
				{currentHost?.agent === false && !empty && (
					<Alert role="status">
						<Info aria-hidden />
						<AlertDescription>
							{copy.restore.noAgent(currentHost.hostname)}
						</AlertDescription>
					</Alert>
				)}
				{detailPane}
				{showBrowser && (
					<FileBrowser
						key={snapshotId}
						snapshot={snapshotId}
						path={path ?? ""}
						q={q}
						selection={selection}
					/>
				)}
			</div>
		);

		let layout: React.ReactNode;
		if (wide) {
			layout = (
				<div
					className={
						empty
							? "flex flex-col gap-6"
							: "grid grid-cols-[320px_minmax(0,1fr)] items-start gap-6"
					}
				>
					{timelineCard && <div className="sticky top-24">{timelineCard}</div>}
					{detailStack}
				</div>
			);
		} else {
			layout = (
				<div className="flex flex-col gap-6">
					{timelineCard && (
						<Collapsible
							open={pickerOpen || !snapshotId}
							onOpenChange={setPickerOpen}
							className="flex flex-col gap-4"
						>
							<div className="flex items-center justify-between gap-4">
								<span
									dir="rtl"
									className="min-w-0 truncate text-left font-semibold"
								>
									<span dir="ltr">
										{current
											? copy.restore.timeline.collapsed(
													formatShortDateTime(current.time),
													shortId(current.id),
												)
											: copy.restore.timeline.title}
									</span>
								</span>
								<CollapsibleTrigger asChild>
									<Button type="button" variant="outline" size="sm">
										{copy.restore.timeline.change}
									</Button>
								</CollapsibleTrigger>
							</div>
							<CollapsibleContent>{timelineCard}</CollapsibleContent>
						</Collapsible>
					)}
					{detailStack}
				</div>
			);
		}
		main = (
			<>
				<div className="flex flex-wrap items-center justify-between gap-4">
					<div className="w-full max-w-90">
						<HostPicker value={hostKey} onChange={selectHost} />
					</div>
					{timeline.data && (
						<p className="text-sm text-muted-foreground">
							{snapshotCount(timeline.data.total)}
						</p>
					)}
				</div>
				{layout}
			</>
		);
	}

	return (
		<div className="flex flex-col gap-6 pb-8">
			<div className="flex flex-col gap-1">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.restore.title}
				</h1>
				<p className="text-sm text-muted-foreground">{copy.restore.lead}</p>
			</div>
			<p role="status" aria-live="polite" className="sr-only">
				{selection.announcement}
			</p>
			{main}
		</div>
	);
}
