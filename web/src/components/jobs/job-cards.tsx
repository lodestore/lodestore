import { Link } from "@tanstack/react-router";
import { Copy } from "lucide-react";
import type { ReactNode } from "react";
import { toast } from "sonner";
import type { JobDetail } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import { formatBytes, formatCount, formatDuration } from "@/lib/format";

const SNAPSHOT_ID_CHARS = 8;

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

function ShortId({ id }: { id: string }) {
	return (
		<span className="inline-flex items-center gap-1">
			<code className="font-mono" title={id}>
				{id.slice(0, SNAPSHOT_ID_CHARS)}
			</code>
			<Button
				type="button"
				variant="ghost"
				size="icon-xs"
				aria-label={copy.common.copy(copy.jobs.cards.snapshotId)}
				onClick={async () => {
					if (await copyText(id)) toast.success(copy.common.copied);
				}}
			>
				<Copy aria-hidden />
			</Button>
		</span>
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

function JobCard({ job }: { job: JobDetail }) {
	const c = copy.jobs.cards;
	const took =
		job.started_at != null && job.finished_at != null
			? formatDuration(job.finished_at - job.started_at)
			: null;
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

export function failureText(job: JobDetail) {
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
	switch (job.state) {
		case "failed":
			return failureText(job);
		case "interrupted":
			return copy.jobs.errors.interrupted;
		case "not_started":
			return copy.jobs.errors.notStarted(job.hostname);
		case "cancelled":
			return copy.jobs.statusHint.cancelled;
		default:
			return null;
	}
}

export function JobCards({ job }: { job: JobDetail }) {
	const text = outcomeText(job);
	const written = job.state === "succeeded" || job.state === "warnings";
	return (
		<div className="grid gap-6 xl:grid-cols-2">
			<JobCard job={job} />
			{written && (
				<div className="flex flex-col gap-4">
					<ResultCard job={job} />
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
					<AlertDescription className="text-current">{text}</AlertDescription>
				</Alert>
			)}
		</div>
	);
}
