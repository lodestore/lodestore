import { Link } from "@tanstack/react-router";
import { RotateCcw } from "lucide-react";
import { useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useGetFindGroup, useGetSnapshot } from "@/api/gen/endpoints";
import type { FindGroup, FindSnapshotRef, FindVersion } from "@/api/gen/model";
import { DownloadDialog } from "@/components/restore/download-dialog";
import { RestoreDialog } from "@/components/restore/restore-dialog";
import {
	OwnerSignedBadge,
	UnverifiedBadge,
} from "@/components/snapshots/suspicious-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatBytes } from "@/lib/format";
import { displayName, pathParts, urlPath } from "@/lib/names";
import { formatMonthDay, formatMonthDayTime } from "./dates";

const shortContentId = (id: string) => `${id.slice(0, 4)}…${id.slice(-4)}`;

const LINK =
	"rounded-sm text-sm text-primary-strong underline underline-offset-4 outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring";

const sameVersion = (a: FindVersion, b: FindVersion) =>
	a.content_id === b.content_id &&
	a.size === b.size &&
	a.mtime_ms === b.mtime_ms;

export function VersionEntry({
	sessionId,
	group,
	version,
	newest,
}: {
	sessionId: string;
	group: FindGroup;
	version: FindVersion;
	newest: boolean;
}) {
	const [dialog, setDialog] = useState<"restore" | "download">();
	const [expanded, setExpanded] = useState(false);
	const restoreButton = useRef<HTMLButtonElement>(null);
	const downloadButton = useRef<HTMLButtonElement>(null);
	const newestRef = version.snapshots[0];
	const snapshot = useGetSnapshot(newestRef?.id ?? "", {
		query: { enabled: dialog !== undefined && newestRef !== undefined },
	});
	const forgotten =
		snapshot.error instanceof ApiError && snapshot.error.status === 404;

	const all = useGetFindGroup(sessionId, group.index, {
		query: { enabled: expanded },
	});
	const chips: FindSnapshotRef[] =
		all.data?.versions.find((v) => sameVersion(v, version))?.snapshots ??
		version.snapshots;

	const first = formatMonthDay(version.first_ms);
	const last = formatMonthDay(version.last_ms);
	const path = displayName(group.path);
	const folder = urlPath(pathParts(group.path_b64).slice(0, -1));
	const showIn = (snapshotId: string) => ({
		host: group.host_key,
		snapshot: snapshotId,
		path: folder || undefined,
		select: group.path_b64,
	});
	const showLabel = (ref: FindSnapshotRef) =>
		copy.find.version.showLabel(
			path,
			formatMonthDayTime(ref.time_ms),
			group.hostname,
		);
	const returnFocus =
		(target: React.RefObject<HTMLButtonElement | null>) => (event: Event) => {
			event.preventDefault();
			target.current?.focus();
		};

	return (
		<li className="group/version relative flex flex-col gap-2 pb-8 pl-6 last:pb-0">
			<span
				aria-hidden
				className="absolute top-2 left-0 size-2 rounded-full bg-primary-strong"
			/>
			<span
				aria-hidden
				className="absolute top-4 bottom-0 left-[3.5px] w-px bg-border group-last/version:hidden"
			/>
			<div className="flex flex-wrap items-center gap-2">
				<p className="font-semibold">{copy.find.range(first, last)}</p>
				{version.verified_by == null && <UnverifiedBadge />}
				{version.verified_by === "owner" && <OwnerSignedBadge />}
			</div>
			<p className="text-sm text-muted-foreground">
				{version.content_id != null && (
					<>
						<span className="font-mono" title={version.content_id}>
							{shortContentId(version.content_id)}
						</span>
						{copy.find.version.separator}
					</>
				)}
				{formatBytes(version.size)}
				{copy.find.version.separator}
				{copy.find.version.modified(formatMonthDayTime(version.mtime_ms))}
			</p>
			<div className="flex flex-wrap items-center gap-2">
				<Button
					ref={restoreButton}
					type="button"
					variant={newest ? "default" : "outline"}
					aria-label={copy.find.version.restoreLabel(
						path,
						group.hostname,
						first,
						last,
					)}
					onClick={() => setDialog("restore")}
				>
					<RotateCcw aria-hidden />
					{copy.find.version.restore}
				</Button>
				{newestRef && (
					<Link
						to="/restore"
						search={showIn(newestRef.id)}
						className={LINK}
						aria-label={showLabel(newestRef)}
					>
						{copy.find.version.show}
					</Link>
				)}
				<button
					ref={downloadButton}
					type="button"
					className={LINK}
					aria-label={copy.find.version.downloadLabel(
						path,
						group.hostname,
						first,
						last,
					)}
					onClick={() => setDialog("download")}
				>
					{copy.find.version.download}
				</button>
			</div>
			{forgotten && (
				<Alert role="alert">
					<AlertDescription>
						<strong className="font-semibold">
							{copy.restore.snapshot.goneHeading}
						</strong>{" "}
						{copy.restore.snapshot.goneBody}
					</AlertDescription>
				</Alert>
			)}
			<p className="text-sm text-muted-foreground">
				{copy.find.version.present(version.snapshot_count)}
			</p>
			<div className="flex flex-wrap items-center gap-2">
				{chips.map((ref) => (
					<Link
						key={ref.id}
						to="/restore"
						search={showIn(ref.id)}
						aria-label={showLabel(ref)}
						className="rounded-md border border-input px-2 py-1 font-mono text-xs outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring"
					>
						{formatMonthDayTime(ref.time_ms)}
					</Link>
				))}
				{expanded && all.isPending ? (
					<Spinner />
				) : (
					version.snapshot_count > chips.length && (
						<button
							type="button"
							className={LINK}
							onClick={() => (all.isError ? all.refetch() : setExpanded(true))}
						>
							{copy.find.version.all(version.snapshot_count)}
						</button>
					)
				)}
			</div>
			<RestoreDialog
				open={dialog === "restore" && !forgotten}
				onOpenChange={(open) => !open && setDialog(undefined)}
				snapshot={snapshot.data}
				items={[group.path_b64]}
				onCloseAutoFocus={returnFocus(restoreButton)}
			/>
			<DownloadDialog
				open={dialog === "download" && !forgotten}
				onOpenChange={(open) => !open && setDialog(undefined)}
				snapshot={snapshot.data}
				items={[group.path_b64]}
				onCloseAutoFocus={returnFocus(downloadButton)}
			/>
		</li>
	);
}
