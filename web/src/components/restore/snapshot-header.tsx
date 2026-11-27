import {
	Box,
	Copy,
	RotateCcw,
	ShieldAlert,
	ShieldQuestion,
} from "lucide-react";
import { memo, useRef, useState } from "react";
import { toast } from "sonner";
import { useContainerPreview } from "@/api/gen/endpoints";
import type { ContainerFacts, RestoreSnapshot } from "@/api/gen/model";
import { ContainerRestoreDialog } from "@/components/restore/container-restore-dialog";
import { SnapshotActionsMenu } from "@/components/snapshots/snapshot-actions-menu";
import {
	type SnapshotAction,
	SnapshotDialogs,
} from "@/components/snapshots/snapshot-dialogs";
import {
	isPendingVerification,
	PolicyDeletedBadge,
	ProvenanceBadge,
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

function Fact({
	label,
	value,
	box,
}: {
	label: string;
	value: string;
	box?: boolean;
}) {
	return (
		<div className="flex flex-col">
			<dt className="text-sm font-semibold text-muted-foreground">{label}</dt>
			<dd className="flex items-center gap-1 tabular-nums">
				{box && <Box aria-hidden className="size-4 text-muted-foreground" />}
				{value}
			</dd>
		</div>
	);
}

const containerText = copy.restore.header;

function containerFacts(facts: ContainerFacts) {
	const mode = containerText.mode as Record<string, string | undefined>;
	return [
		{
			label: containerText.source,
			value: containerText.container(facts.vmid, facts.name),
			box: true,
		},
		{ label: containerText.modeLabel, value: mode[facts.mode] ?? facts.mode },
		{
			label: containerText.volumes,
			value: facts.volumes
				.map((v) => containerText.volume(v.key, formatBytes(v.size_bytes)))
				.join(" · "),
		},
		{
			label: containerText.userIds.label,
			value: facts.unprivileged
				? containerText.userIds.unprivileged(facts.root_host_uid)
				: containerText.userIds.privileged,
		},
	];
}

export const SnapshotHeader = memo(function SnapshotHeader({
	snapshot,
}: {
	snapshot?: RestoreSnapshot;
}) {
	const [action, setAction] = useState<SnapshotAction>();
	const [restoring, setRestoring] = useState(false);
	const restoreButton = useRef<HTMLButtonElement>(null);
	const isContainer = snapshot?.source_type === "proxmox-ct";
	const container = useContainerPreview(snapshot?.id ?? "", undefined, {
		query: { enabled: isContainer },
	}).data;
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
								<ProvenanceBadge snapshot={snapshot} />
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
							{isContainer && container ? (
								containerFacts(container).map((fact) => (
									<Fact key={fact.label} {...fact} />
								))
							) : (
								<Fact
									label={copy.restore.header.source}
									value={snapshotSource(snapshot)}
								/>
							)}
							<Fact
								label={copy.restore.header.size}
								value={formatBytes(snapshot.stats.bytes_total)}
							/>
							{!isContainer && (
								<Fact
									label={copy.restore.header.files}
									value={formatCount(snapshotFiles(snapshot))}
								/>
							)}
						</dl>
					</div>
					{isContainer && (
						<Button
							ref={restoreButton}
							type="button"
							onClick={() => setRestoring(true)}
						>
							<RotateCcw aria-hidden />
							{copy.restore.container.open}
						</Button>
					)}
					<SnapshotActionsMenu snapshot={snapshot} onAction={setAction} />
				</div>
				{snapshot.incomplete && (
					<p className="text-sm text-muted-foreground">
						{copy.restore.header.incompleteBody}
					</p>
				)}
			</div>
			{snapshot.verified_by == null && !isPendingVerification(snapshot) && (
				<Alert role="status">
					<ShieldQuestion aria-hidden className="text-warning" />
					<AlertTitle>{copy.restore.header.unverifiedTitle}</AlertTitle>
					<AlertDescription>
						{copy.restore.header.unverifiedBody}
					</AlertDescription>
				</Alert>
			)}
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
			{isContainer && (
				<ContainerRestoreDialog
					open={restoring}
					onOpenChange={setRestoring}
					snapshot={snapshot}
					onCloseAutoFocus={(event) => {
						event.preventDefault();
						restoreButton.current?.focus();
					}}
				/>
			)}
			<SnapshotDialogs
				target={action ? { snapshot, action } : undefined}
				onClose={() => setAction(undefined)}
			/>
		</div>
	);
});
