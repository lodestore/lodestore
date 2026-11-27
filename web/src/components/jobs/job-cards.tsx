import { Link } from "@tanstack/react-router";
import {
	Copy,
	Lock,
	Network,
	Power,
	RefreshCw,
	ShieldAlert,
	TriangleAlert,
} from "lucide-react";
import type { ReactNode } from "react";
import { toast } from "sonner";
import { useContainerPreview } from "@/api/gen/endpoints";
import type { JobDetail } from "@/api/gen/model";
import { PathText, TextWithPaths } from "@/components/path-text";
import { DroppedKeys, KeyRows } from "@/components/restore/dropped-keys";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import {
	formatBytes,
	formatCount,
	formatDuration,
	formatRate,
} from "@/lib/format";
import { itemCount } from "@/lib/plural";
import { shortId } from "@/lib/snapshot";
import { sourceTypeNoun } from "@/lib/source-type";

type Fact = [label: string, value: ReactNode | null | undefined];

const present = (value: Fact[1]) =>
	value !== null && value !== undefined && value !== "";

function dateTime(ms?: number | null) {
	if (ms == null) return null;
	return (
		<time dateTime={new Date(ms).toISOString()}>
			{new Date(ms).toLocaleString("en", {
				dateStyle: "medium",
				timeStyle: "medium",
			})}
		</time>
	);
}

function Facts({ title, facts }: { title: string; facts: Fact[] }) {
	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2>{title}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent>
				<dl className="grid grid-cols-[8rem_1fr] items-center gap-x-4 gap-y-2 text-sm">
					{facts.map(([label, value]) => (
						<div key={label} className="contents">
							<dt className="text-muted-foreground">{label}</dt>
							<dd className="min-w-0 wrap-anywhere">
								{present(value) ? value : copy.common.dash}
							</dd>
						</div>
					))}
				</dl>
			</CardContent>
		</Card>
	);
}

function CopyCode({
	value,
	shown = value,
	label,
}: {
	value: string;
	shown?: string;
	label: string;
}) {
	return (
		<span className="inline-flex items-center gap-1">
			<code className="font-mono" title={value}>
				<PathText value={shown} />
			</code>
			<Button
				type="button"
				variant="ghost"
				size="icon-xs"
				aria-label={copy.common.copy(label)}
				onClick={async () => {
					if (await copyText(value)) toast.success(copy.common.copied);
				}}
			>
				<Copy aria-hidden />
			</Button>
		</span>
	);
}

const ShortId = ({ id }: { id: string }) => (
	<CopyCode value={id} shown={shortId(id)} label={copy.jobs.cards.snapshotId} />
);

const isAgentless = (hostId: string) => hostId.startsWith("agentless:");

function HostLink({ id, name }: { id: string; name: string }) {
	if (isAgentless(id)) return name;
	return (
		<Link
			to="/hosts/$id"
			params={{ id }}
			className="rounded-sm text-primary-strong"
		>
			{name}
		</Link>
	);
}

function SnapshotLink({ host, id }: { host: string; id: string }) {
	return (
		<Link
			to="/restore"
			search={{ host, snapshot: id }}
			className="rounded-sm text-primary-strong"
		>
			<code className="font-mono" title={id}>
				{shortId(id)}
			</code>
		</Link>
	);
}

function sourceText(job: JobDetail) {
	const c = copy.jobs.cards;
	if (job.source === "cli") return c.sourceCli(job.caller_uid ?? 0);
	switch (job.origin) {
		case "schedule":
			return c.sourceSchedule;
		case "catchup":
			return c.sourceCatchup;
		default:
			return c.sourceRunNow;
	}
}

function limitsText(job: JobDetail) {
	const c = copy.jobs.cards;
	const limits = job.limits;
	if (!limits) return null;
	const parts = [
		limits.upload_rate_bps
			? c.limitUpload(formatRate(limits.upload_rate_bps))
			: "",
		limits.nice != null ? c.limitNice(limits.nice) : "",
		limits.ionice_class
			? c.limitIo(
					c.ioClass[limits.ionice_class as keyof typeof c.ioClass] ??
						limits.ionice_class,
				)
			: "",
	].filter(Boolean);
	return parts.length > 0 ? parts.join(" · ") : null;
}

function SuspiciousChip() {
	return (
		<Badge variant="outline" className="text-warning" data-status="suspicious">
			<ShieldAlert aria-hidden />
			{copy.jobs.cards.suspiciousBadge}
		</Badge>
	);
}

function retentionValue(job: JobDetail) {
	const c = copy.jobs.cards;
	const r = job.retention;
	if (!r || r.state !== "done") return null;
	const forgot = r.forgot ?? 0;
	const blocked = r.blocked ?? [];
	if (blocked.length > 0) {
		const reasons = [...new Set(blocked.map((b) => b.reason))];
		return (
			<Tooltip>
				<TooltipTrigger asChild>
					<span tabIndex={0} className="inline-flex items-center gap-2">
						<TriangleAlert
							aria-hidden
							className="size-4 shrink-0 text-warning"
						/>
						{c.retentionBlocked(forgot, blocked.length)}
					</span>
				</TooltipTrigger>
				<TooltipContent className="flex flex-col gap-1">
					{reasons.map((reason) => (
						<span key={reason}>
							<TextWithPaths text={reason} />
						</span>
					))}
				</TooltipContent>
			</Tooltip>
		);
	}
	return forgot === 0
		? c.retentionNothing
		: c.retentionForgot(forgot, r.kept ?? 0);
}

function policyValue(job: JobDetail) {
	if (job.source === "cli" || !job.policy_name) return null;
	if (!job.policy_exists) return job.policy_name;
	return (
		<Link
			to="/policies/$name"
			params={{ name: job.policy_name }}
			className="rounded-sm text-primary-strong"
		>
			{job.policy_name}
		</Link>
	);
}

const tookText = (job: JobDetail) =>
	job.started_at != null && job.finished_at != null
		? formatDuration(job.finished_at - job.started_at)
		: null;

/** The "CT 200 · dns" the snapshot came from (null while unknown) and the "CT 105 · dns" it became. */
export function useContainerLabels(job: JobDetail) {
	const guest = job.restore?.guest;
	const facts = useContainerPreview(job.restore?.snapshot_id ?? "", undefined, {
		query: { enabled: !!guest, retry: false },
	}).data;
	const name =
		job.restore_result?.guest?.name || (guest?.replace ? null : facts?.name);
	return {
		source: facts
			? copy.restore.header.container(facts.vmid, facts.name)
			: null,
		target: copy.restore.header.container(guest?.vmid, name),
	};
}

function restoreFacts(
	job: JobDetail,
	labels: ReturnType<typeof useContainerLabels>,
): Fact[] {
	const c = copy.jobs.cards;
	const restore = job.restore;
	const guest = restore?.guest;
	const source = restore?.source_host_key ?? job.host_id;
	const snapshot = restore && (
		<SnapshotLink key="snapshot" host={source} id={restore.snapshot_id} />
	);
	if (restore && guest) {
		return [
			[c.node, <HostLink key="host" id={job.host_id} name={guest.node} />],
			[c.snapshot, snapshot],
			[
				c.source,
				labels.source && c.sourceOnNode(labels.source, restore.source_hostname),
			],
			[c.container, c.containerValue(labels.target, guest.storage)],
			[c.startedBy, restore.started_by],
			[c.duration, tookText(job)],
		];
	}
	return [
		[c.host, <HostLink key="host" id={job.host_id} name={job.hostname} />],
		[c.snapshot, snapshot],
		[
			c.sourceHost,
			restore && (
				<HostLink key="source" id={source} name={restore.source_hostname} />
			),
		],
		[
			c.destination,
			restore && (
				<CopyCode
					key="destination"
					value={`${job.hostname}:${restore.destination}`}
					label={c.copyDestination}
				/>
			),
		],
		[c.ifExists, restore && c.conflict[restore.conflict]],
		[
			c.owners,
			restore && (restore.restore_owners ? c.ownersOriginal : c.ownersAgent),
		],
		[c.startedBy, restore?.started_by],
		[c.queued, dateTime(job.queued_at)],
		[c.started, dateTime(job.started_at)],
		[c.finished, dateTime(job.finished_at)],
		[c.duration, tookText(job)],
	];
}

function RestoreJobCard({ job }: { job: JobDetail }) {
	const labels = useContainerLabels(job);
	return (
		<Facts title={copy.jobs.cards.job} facts={restoreFacts(job, labels)} />
	);
}

function DownloadJobCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const download = job.download;
	return (
		<Facts
			title={c.job}
			facts={[
				[c.host, <HostLink key="host" id={job.host_id} name={job.hostname} />],
				[
					c.snapshot,
					download && (
						<SnapshotLink
							key="snapshot"
							host={job.host_id}
							id={download.snapshot_id}
						/>
					),
				],
				[c.items, download && itemCount(download.items)],
				[c.format, download && copy.jobs.archiveFormat[download.format]],
				[c.startedBy, download?.started_by],
				[c.started, dateTime(job.started_at)],
				[c.finished, dateTime(job.finished_at)],
				[c.duration, tookText(job)],
			]}
		/>
	);
}

function JobCard({ job }: { job: JobDetail }) {
	if (job.kind === "restore") return <RestoreJobCard job={job} />;
	if (job.kind === "download") return <DownloadJobCard job={job} />;
	const c = copy.jobs.cards;
	const took = tookText(job);
	const limits = limitsText(job);
	return (
		<Facts
			title={c.job}
			facts={[
				[
					c.host,
					<Link
						key="host"
						to="/hosts/$id"
						params={{ id: job.host_id }}
						className="rounded-sm text-primary-strong"
					>
						{job.hostname}
					</Link>,
				],
				[c.policy, policyValue(job)],
				[c.source, sourceText(job)],
				...(limits ? [[c.limits, limits] as Fact] : []),
				[c.queued, dateTime(job.queued_at)],
				[c.started, dateTime(job.started_at)],
				[c.finished, dateTime(job.finished_at)],
				[c.duration, took],
			]}
		/>
	);
}

function ResultCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const stats = job.stats;
	const files = stats
		? (stats.files_new ?? 0) +
			(stats.files_changed ?? 0) +
			(stats.files_unchanged ?? 0)
		: null;
	const retention = retentionValue(job);
	return (
		<Facts
			title={c.result}
			facts={[
				[
					c.snapshot,
					job.snapshot_id ? (
						<span key="snapshot" className="flex flex-wrap items-center gap-2">
							<ShortId id={job.snapshot_id} />
							{job.incomplete && (
								<Badge variant="outline" className="text-warning">
									{c.incompleteBadge}
								</Badge>
							)}
							{job.snapshot_suspicious && <SuspiciousChip />}
							<Link
								to="/hosts/$id"
								params={{ id: job.host_id }}
								search={{ tab: "snapshots" }}
								className="rounded-sm text-primary-strong"
							>
								{c.viewSnapshots}
							</Link>
							<Link
								to="/restore"
								search={{ host: job.host_id, snapshot: job.snapshot_id }}
								className="rounded-sm text-primary-strong"
							>
								{c.browseFiles}
							</Link>
						</span>
					) : null,
				],
				[c.size, stats ? formatBytes(stats.bytes_total ?? 0) : null],
				[c.files, files === null ? null : formatCount(files)],
				[c.uploaded, stats ? formatBytes(stats.bytes_added ?? 0) : null],
				[
					c.parent,
					job.snapshot_id ? (
						job.parent_id ? (
							<ShortId key="parent" id={job.parent_id} />
						) : (
							c.parentNone
						)
					) : null,
				],
				...(retention ? [[c.retention, retention] as Fact] : []),
			]}
		/>
	);
}

export function restoreFailureText(
	job: Pick<JobDetail, "hostname" | "restore" | "error_code" | "error_message">,
) {
	const host = job.hostname;
	const path = job.restore?.destination ?? "";
	const e = copy.jobs.errors;
	const guest = job.restore?.guest;
	if (guest) {
		const vmid = guest.vmid;
		const own = job.error_message?.trim() ?? "";
		switch (job.error_code) {
			case "guest_running":
				return own || e.guestRunning(vmid);
			case "guest_ha_managed":
				return own || e.guestHaManaged(vmid);
			case "guest_target_changed":
				return e.guestTargetChanged(vmid);
			case "restore_expired":
				return e.restoreExpired;
			case "guest_plugin_missing":
				return e.guestPluginMissing(guest.node);
			case "guest_in_place_disabled":
				return e.guestInPlaceDisabled(guest.node);
			case "restore_vmid_taken":
				return e.restoreVmidTaken(vmid);
			case "restore_snapshot_not_ct":
				return e.restoreSnapshotNotCt;
			case "restore_no_space":
				return e.restoreFailed(own).replace(/\s{2,}/g, " ");
		}
	}
	switch (job.error_code) {
		case "restore_target_unwritable":
			return e.restore_target_unwritable(host, path);
		case "restore_target_not_empty":
			return e.restore_target_not_empty(host, path);
		case "restore_disk_full":
			return e.restore_disk_full(host, path);
		case "restore_no_space":
			return e.restore_no_space(host, path);
		case "restore_outside_roots":
			return e.restore_outside_roots(host, path);
		case "restore_in_place_disabled":
			return e.restore_in_place_disabled(host);
		case "restore_data_missing":
			return e.restore_data_missing;
		case "restore_integrity":
			return e.restore_integrity;
		case "restore_access_denied":
			return e.restore_access_denied(host);
	}
	return e
		.restoreFailed(job.error_message?.trim() ?? "")
		.replace(/\s{2,}/g, " ");
}

/** Friendly text for a backup error code, or null when the code is unknown
 * or its details are missing. The jobs list has no `error_detail`. */
export function backupFailureCopy(
	job: Pick<JobDetail, "hostname" | "error_code"> & {
		error_detail?: JobDetail["error_detail"];
	},
): string | null {
	const host = job.hostname;
	const detail = job.error_detail ?? {};
	const e = copy.jobs.errors;
	const has = (...keys: string[]) => keys.every((key) => detail[key]);
	switch (job.error_code) {
		case "path_missing":
			if (has("path")) return e.path_missing(detail.path, host);
			break;
		case "pre_hook_failed":
			if (has("script", "code")) {
				return e.pre_hook_failed(detail.script, Number(detail.code));
			}
			break;
		case "hook_rejected":
			if (has("script")) return e.hook_rejected(detail.script, host);
			break;
		case "hook_missing":
			if (has("script")) return e.hook_missing(detail.script, host);
			break;
		case "inline_not_allowed":
			return e.inline_not_allowed(host);
		case "guest_plugin_missing":
			return e.guestPluginMissing(host);
		case "guest_not_local":
			return e.guestNotLocal(host);
		case "source_unknown":
			return e.sourceUnknown(host);
		case "ct_mode_mismatch":
			return e.ctModeMismatch;
		case "gc_agent_outdated":
			return e.gc_agent_outdated(host);
		case "source_agent_outdated": {
			const noun = sourceTypeNoun(detail.source_type ?? "");
			return noun
				? e.source_agent_outdated(host, noun)
				: e.source_agent_outdated_any(host);
		}
		case "source_not_found": {
			const noun = sourceTypeNoun(detail.source_type ?? "");
			return noun
				? e.source_not_found(host, noun)
				: e.source_not_found_any(host);
		}
		case "hook_timeout":
			if (has("script", "timeout")) {
				return e.hook_timeout(detail.script, detail.timeout);
			}
			break;
	}
	return null;
}

/** Short list form for failures whose full text opens with a generic lead. */
export function backupFailureShort(
	job: Pick<JobDetail, "error_code"> & {
		error_detail?: JobDetail["error_detail"];
	},
): string | null {
	const s = copy.jobs.errors.short;
	const noun = sourceTypeNoun(job.error_detail?.source_type ?? "");
	switch (job.error_code) {
		case "source_agent_outdated":
			return noun ? s.source_agent_outdated(noun) : s.source_agent_outdated_any;
		case "source_not_found":
			return noun ? s.source_not_found(noun) : s.source_not_found_any;
	}
	return null;
}

export function failureText(job: JobDetail) {
	if (job.kind === "restore") return restoreFailureText(job);
	return (
		backupFailureCopy(job) ??
		copy.jobs.errors
			.failed(job.error_message?.trim() ?? "")
			.replace(/\s{2,}/g, " ")
	);
}

function outcomeText(job: JobDetail) {
	const e = copy.jobs.errors;
	const kind = job.kind;
	switch (job.state) {
		case "failed":
			return failureText(job);
		case "interrupted":
			return kind === "download"
				? e.downloadInterrupted
				: kind === "restore"
					? e.restoreInterrupted
					: e.interrupted;
		case "not_started":
			return kind === "download"
				? e.downloadNotStarted
				: kind === "restore"
					? e.restoreNotStarted(job.hostname)
					: e.notStarted(job.hostname);
		case "cancelled":
			if (job.error_code === "coalesced") return e.coalesced;
			return kind === "restore"
				? copy.jobs.otherStatusHint.cancelled
				: copy.jobs.statusHint.cancelled;
		default:
			return null;
	}
}

function ContainerResultCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const e = c.containerEffects;
	const guest = job.restore_result?.guest;
	const info = job.restore?.guest;
	const vmid = guest?.vmid ?? info?.vmid ?? 0;
	const nets = guest?.new_mac_nets ?? [];
	const effects = [
		{
			Icon: Power,
			label:
				(guest?.replaced ?? info?.replace) ? e.replaced(vmid) : e.stopped(vmid),
		},
		...(nets.length > 0
			? [{ Icon: RefreshCw, label: e.newMacs(nets.join(", ")) }]
			: []),
		...(guest?.unprivileged
			? [{ Icon: Lock, label: e.unprivileged(guest.root_host_uid) }]
			: []),
	];
	const network = guest?.network ?? [];
	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2>{c.result}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				<ul className="flex flex-col divide-y">
					{effects.map(({ Icon, label }) => (
						<li key={label} className="flex items-center gap-2 py-2 text-sm">
							<Icon
								aria-hidden
								className="size-4 shrink-0 text-muted-foreground"
							/>
							{label}
						</li>
					))}
				</ul>
				{network.length > 0 && (
					<div className="flex flex-col gap-1">
						<h3 className="text-sm font-semibold">{c.networkKept}</h3>
						<KeyRows rows={network} icon={Network} />
					</div>
				)}
			</CardContent>
		</Card>
	);
}

function ContainerDroppedCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const guest = job.restore_result?.guest;
	const dropped = guest?.dropped ?? [];
	if (dropped.length === 0) return null;
	const total = guest?.dropped_total ?? dropped.length;
	return (
		<Card className="xl:col-span-2">
			<CardHeader>
				<CardTitle>
					<h2>{c.droppedTitle(total)}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				<DroppedKeys dropped={dropped} />
				{total > dropped.length && (
					<p className="text-sm text-muted-foreground">
						{c.droppedCut(dropped.length)}
					</p>
				)}
				<p className="text-sm text-muted-foreground">
					{c.droppedHint}{" "}
					<a
						href={c.docsUrl}
						target="_blank"
						rel="noreferrer"
						className="rounded-sm text-primary-strong"
					>
						{c.docsLabel}
					</a>
				</p>
			</CardContent>
		</Card>
	);
}

function RestoreResultCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	if (job.restore?.guest) return <ContainerResultCard job={job} />;
	const result = job.restore_result;
	const count = (n?: number) => (result ? formatCount(n ?? 0) : null);
	return (
		<Facts
			title={c.result}
			facts={[
				[c.created, count(result?.created)],
				[c.overwritten, count(result?.overwritten)],
				[c.skipped, count(result?.skipped)],
				[c.warnings, formatCount(job.warnings_total)],
			]}
		/>
	);
}

function DownloadResultCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	return (
		<Facts
			title={c.result}
			facts={[[c.sent, formatBytes(job.download?.bytes_sent ?? 0)]]}
		/>
	);
}

const active = (job: JobDetail) =>
	job.state === "queued" || job.state === "running";

function resultCard(job: JobDetail) {
	const written = job.state === "succeeded" || job.state === "warnings";
	switch (job.kind) {
		case "restore":
			return written || (job.restore_result && !active(job)) ? (
				<RestoreResultCard job={job} />
			) : null;
		case "download":
			return written || job.state === "interrupted" ? (
				<DownloadResultCard job={job} />
			) : null;
		default:
			return written ? <ResultCard job={job} /> : null;
	}
}

function AgentUpdateCards({ job }: { job: JobDetail }) {
	const text = copy.jobs.agentUpdate;
	const info = job.agent_update;
	const reason = info?.reason?.trim() ?? "";
	const note =
		job.state === "failed"
			? text.reason(reason)
			: job.state === "cancelled" && reason
				? text.cancelledReason(reason)
				: null;
	return (
		<div className="grid gap-6 xl:grid-cols-2">
			<Facts
				title={text.cardTitle}
				facts={[
					[
						copy.jobs.cards.host,
						<HostLink id={job.host_id} name={job.hostname} />,
					],
					[text.from, info?.from_version],
					[text.to, info?.to_version],
					[text.queued, dateTime(job.queued_at)],
					[text.started, dateTime(job.started_at)],
					[text.finished, dateTime(job.finished_at)],
				]}
			/>
			{note !== null && (
				<Alert
					variant={job.state === "failed" ? "destructive" : "default"}
					role={job.state === "failed" ? "alert" : "note"}
					className="self-start wrap-anywhere"
				>
					<AlertDescription className="text-current">
						<TextWithPaths text={note} />
					</AlertDescription>
				</Alert>
			)}
		</div>
	);
}

export function JobCards({ job }: { job: JobDetail }) {
	if (job.kind === "agent_update") return <AgentUpdateCards job={job} />;
	const text = outcomeText(job);
	const result = resultCard(job);
	return (
		<div className="grid gap-6 xl:grid-cols-2">
			<JobCard job={job} />
			{result && (
				<div className="flex flex-col gap-4">
					{result}
					{job.incomplete && (
						<p className="text-sm text-muted-foreground">
							{copy.jobs.cards.incomplete}
						</p>
					)}
					{job.kind === "backup" &&
						job.snapshot_suspicious &&
						job.snapshot_id && (
							<p className="text-sm text-muted-foreground">
								{copy.jobs.cards.suspiciousNote}{" "}
								<Link
									to="/restore"
									search={{ host: job.host_id, snapshot: job.snapshot_id }}
									className="rounded-sm text-primary-strong"
								>
									{copy.jobs.cards.reviewSnapshot}
								</Link>
							</p>
						)}
				</div>
			)}
			{job.kind === "restore" && job.restore?.guest && (
				<ContainerDroppedCard job={job} />
			)}
			{text !== null && (
				<Alert
					variant={job.state === "failed" ? "destructive" : "default"}
					role={job.state === "failed" ? "alert" : "note"}
					className="self-start wrap-anywhere"
				>
					<AlertDescription className="text-current">
						<TextWithPaths text={text} />
					</AlertDescription>
				</Alert>
			)}
		</div>
	);
}
