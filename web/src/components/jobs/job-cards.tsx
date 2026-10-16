import { Link } from "@tanstack/react-router";
import { Copy } from "lucide-react";
import type { ReactNode } from "react";
import { toast } from "sonner";
import type { JobDetail } from "@/api/gen/model";
import { PathText, TextWithPaths } from "@/components/path-text";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import { formatBytes, formatCount, formatDuration } from "@/lib/format";
import { itemCount } from "@/lib/plural";
import { shortId } from "@/lib/snapshot";

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

const sourceText = (job: JobDetail) =>
	job.source === "cli"
		? copy.jobs.cards.sourceCli(job.caller_uid ?? 0)
		: copy.jobs.cards.sourceRunNow;

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

function RestoreJobCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const restore = job.restore;
	const source = restore?.source_host_key ?? job.host_id;
	return (
		<Facts
			title={c.job}
			facts={[
				[c.host, <HostLink key="host" id={job.host_id} name={job.hostname} />],
				[
					c.snapshot,
					restore && (
						<SnapshotLink
							key="snapshot"
							host={source}
							id={restore.snapshot_id}
						/>
					),
				],
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
					restore &&
						(restore.restore_owners ? c.ownersOriginal : c.ownersAgent),
				],
				[c.startedBy, restore?.started_by],
				[c.queued, dateTime(job.queued_at)],
				[c.started, dateTime(job.started_at)],
				[c.finished, dateTime(job.finished_at)],
				[c.duration, tookText(job)],
			]}
		/>
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
	switch (job.error_code) {
		case "restore_target_unwritable":
			return e.restore_target_unwritable(host, path);
		case "restore_target_not_empty":
			return e.restore_target_not_empty(host, path);
		case "restore_disk_full":
			return e.restore_disk_full(host, path);
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

export function failureText(job: JobDetail) {
	if (job.kind === "restore") return restoreFailureText(job);
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
		case "hook_timeout":
			if (has("script", "timeout")) {
				return e.hook_timeout(detail.script, detail.timeout);
			}
			break;
	}
	return e.failed(job.error_message?.trim() ?? "").replace(/\s{2,}/g, " ");
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
			return kind === "restore"
				? copy.jobs.otherStatusHint.cancelled
				: copy.jobs.statusHint.cancelled;
		default:
			return null;
	}
}

function RestoreResultCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
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

export function JobCards({ job }: { job: JobDetail }) {
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
				</div>
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
