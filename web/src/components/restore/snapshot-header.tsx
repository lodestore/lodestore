import { Copy, ShieldAlert } from "lucide-react";
import { memo, useState } from "react";
import { toast } from "sonner";
import type { RestoreSnapshot } from "@/api/gen/model";
import { SnapshotActionsMenu } from "@/components/snapshots/snapshot-actions-menu";
import {
	type SnapshotAction,
	SnapshotDialogs,
} from "@/components/snapshots/snapshot-dialogs";
import {
	PolicyDeletedBadge,
	SuspiciousBadge,
	suspicionReasons,
} from "@/components/snapshots/suspicious-badge";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import { formatBytes, formatCount, formatDateTime } from "@/lib/format";
import { shortId, snapshotFiles, snapshotSource } from "@/lib/snapshot";

function Fact({ label, value }: { label: string; value: string }) {
	return (
		<div className="flex flex-col">
			<dt className="text-sm font-semibold text-muted-foreground">{label}</dt>
			<dd className="tabular-nums">{value}</dd>
		</div>
	);
}

export const SnapshotHeader = memo(function SnapshotHeader({
	snapshot,
}: {
	snapshot?: RestoreSnapshot;
}) {
	const [action, setAction] = useState<SnapshotAction>();
	if (snapshot === undefined) {
		return (
			<div
				aria-hidden
				className="flex flex-wrap items-center justify-between gap-4 rounded-lg border bg-card p-4"
			>
				<div className="flex flex-col gap-2">
					<Skeleton className="h-6 w-48" />
					<Skeleton className="h-4 w-32" />
				</div>
				<Skeleton className="h-10 w-72" />
			</div>
		);
	}
	const reasons = suspicionReasons(snapshot);
	return (
		<div className="flex flex-col gap-4">
			<div className="flex flex-col gap-2 rounded-lg border bg-card p-4">
				<div className="flex items-start justify-between gap-4">
					<div className="flex min-w-0 flex-1 flex-wrap items-center justify-between gap-4">
						<div className="flex min-w-0 flex-col">
							<div className="flex flex-wrap items-center gap-2">
								<h2 className="text-base font-semibold">
									{copy.restore.header.title}{" "}
									<span className="font-mono font-normal">
										{shortId(snapshot.id)}
									</span>
								</h2>
								<Button
									type="button"
									variant="ghost"
									size="icon-xs"
									aria-label={copy.restore.header.copyId}
									onClick={async () => {
										if (await copyText(snapshot.id)) {
											toast.success(copy.common.copied);
										}
									}}
								>
									<Copy aria-hidden />
								</Button>
								{snapshot.suspicious && <SuspiciousBadge snapshot={snapshot} />}
								{snapshot.incomplete && (
									<StatusBadge status="warning">
										{copy.restore.header.incomplete}
									</StatusBadge>
								)}
								{snapshot.policy_deleted && <PolicyDeletedBadge />}
							</div>
							<p className="text-sm text-muted-foreground">
								{formatDateTime(snapshot.time)}
							</p>
						</div>
						<dl className="flex flex-wrap gap-6">
							<Fact
								label={copy.restore.header.source}
								value={snapshotSource(snapshot)}
							/>
							<Fact
								label={copy.restore.header.size}
								value={formatBytes(snapshot.stats.bytes_total)}
							/>
							<Fact
								label={copy.restore.header.files}
								value={formatCount(snapshotFiles(snapshot))}
							/>
						</dl>
					</div>
					<SnapshotActionsMenu snapshot={snapshot} onAction={setAction} />
				</div>
				{snapshot.incomplete && (
					<p className="text-sm text-muted-foreground">
						{copy.restore.header.incompleteBody}
					</p>
				)}
			</div>
			{snapshot.suspicious && (
				<Alert role="status">
					<ShieldAlert aria-hidden className="text-warning" />
					<AlertTitle>{copy.restore.header.suspiciousTitle}</AlertTitle>
					<AlertDescription className="flex flex-col gap-2">
						{reasons.length > 0 && (
							<ul className="flex flex-col gap-1 wrap-anywhere">
								{reasons.map((reason, index) => (
									<li key={`${index}-${reason}`}>{reason}</li>
								))}
							</ul>
						)}
						<p>{copy.restore.header.suspiciousBody}</p>
						<div className="flex flex-wrap gap-2">
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={() => setAction("clear")}
							>
								{copy.restore.header.clearMark}
							</Button>
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={() => setAction("delete")}
							>
								{copy.restore.header.deleteSnapshot}
							</Button>
						</div>
					</AlertDescription>
				</Alert>
			)}
			<SnapshotDialogs
				target={action ? { snapshot, action } : undefined}
				onClose={() => setAction(undefined)}
			/>
		</div>
	);
});
