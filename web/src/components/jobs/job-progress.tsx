import { Gauge, TriangleAlert } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import type { JobDetail } from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import {
	formatBytes,
	formatCount,
	formatRate,
	formatTimeLeft,
	percent,
} from "@/lib/format";
import { warningCount } from "@/lib/plural";

const STALE_MS = 15_000;
const TICK_MS = 1_000;

export const formatClock = (at: number) =>
	new Date(at).toLocaleTimeString("en", { timeStyle: "medium" });

const THROTTLED_AT = 0.9;

export const retentionRunning = (job: Pick<JobDetail, "retention">) =>
	job.retention?.state === "running";

export function phaseLabel(
	job: Pick<JobDetail, "state" | "phase" | "kind" | "retention">,
) {
	const p = copy.jobs.phases;
	if (job.kind === "backup" && retentionRunning(job)) return p.retention;
	if (job.kind === "download") {
		return job.state === "queued" ? p.download_queued : p.download_stream;
	}
	if (job.state === "queued") return p.queued;
	if (job.kind === "restore") {
		switch (job.phase) {
			case "restore_files":
				return p.restore_files;
			case "restore_metadata":
				return p.restore_metadata;
			case "restore_plan":
			case null:
			case undefined:
				return p.restore_plan;
			default:
				return p.restore_finishing;
		}
	}
	switch (job.phase) {
		case "pre_hook":
			return copy.jobs.phases.pre_hook;
		case "post_hook":
			return copy.jobs.phases.post_hook;
		case "snapshot":
			return copy.jobs.phases.finalize;
		default:
			return copy.jobs.phases.backup;
	}
}

function useNow(active: boolean) {
	const [now, setNow] = useState(Date.now);
	useEffect(() => {
		if (!active) return;
		setNow(Date.now());
		const timer = setInterval(() => setNow(Date.now()), TICK_MS);
		return () => clearInterval(timer);
	}, [active]);
	return now;
}

function useSeenAt(value: number | undefined) {
	const [seen, setSeen] = useState(() => ({ value, at: Date.now() }));
	if (seen.value !== value) setSeen({ value, at: Date.now() });
	return seen.at;
}

function Metric({
	label,
	value,
	sub,
	badge,
}: {
	label: string;
	value: string;
	sub?: string;
	badge?: ReactNode;
}) {
	return (
		<Card size="sm" className="gap-1 px-4">
			<dt className="text-sm text-muted-foreground">{label}</dt>
			<dd className="flex flex-col">
				<span className="flex flex-wrap items-center gap-x-2">
					<span className="text-2xl font-semibold tabular-nums">{value}</span>
					{badge}
				</span>
				<span className="min-h-6 text-sm text-muted-foreground">{sub}</span>
			</dd>
		</Card>
	);
}

export function JobProgress({
	job,
	onShowWarnings,
}: {
	job: JobDetail;
	onShowWarnings: () => void;
}) {
	const running = job.state === "running";
	const now = useNow(running);
	const progress = job.progress;
	const seenAt = useSeenAt(progress?.updated_at);
	const restore = job.kind === "restore";
	const download = job.kind === "download";
	const scanned = running && (restore || progress?.scan_done === true);
	const pct = scanned ? percent(progress ?? {}) : null;
	const sent = progress?.bytes_done || job.download?.bytes_sent || 0;
	const limit = restore ? 0 : (job.limits?.upload_rate_bps ?? 0);
	const uploadSpeed = progress?.upload_speed_bps ?? 0;
	const stale =
		running &&
		job.host_online &&
		progress != null &&
		(progress.updated_at ?? 0) > 0 &&
		now - seenAt > STALE_MS;

	if (download && running) {
		return (
			<Card>
				<CardContent className="flex flex-col gap-4">
					<h2 className="text-sm font-semibold">{phaseLabel(job)}</h2>
					<dl aria-live="off" className="grid grid-cols-2 gap-4">
						<Metric
							label={copy.jobs.metrics.sent}
							value={sent > 0 ? formatBytes(sent) : copy.common.dash}
						/>
						<Metric
							label={copy.jobs.metrics.speed}
							value={
								progress?.speed_bps
									? formatRate(progress.speed_bps)
									: copy.common.dash
							}
							sub={copy.jobs.metrics.speedSub}
						/>
					</dl>
				</CardContent>
			</Card>
		);
	}

	return (
		<Card>
			<CardContent className="flex flex-col gap-4">
				<div className="flex flex-wrap items-center gap-2">
					<h2 className="text-sm font-semibold">{phaseLabel(job)}</h2>
					{job.warnings_total > 0 && (
						<Button
							type="button"
							variant="outline"
							size="xs"
							className="text-warning"
							onClick={onShowWarnings}
						>
							<TriangleAlert aria-hidden />
							{warningCount(job.warnings_total)}
						</Button>
					)}
					{stale && progress && (
						<span className="ml-auto text-sm text-muted-foreground">
							{copy.jobs.detail.lastUpdate(
								formatClock(progress.updated_at ?? 0),
							)}
						</span>
					)}
				</div>
				{running && (
					<>
						<div className="flex items-center gap-4">
							<Progress
								value={pct}
								aria-label={
									restore
										? copy.jobs.result.restoreProgressLabel
										: copy.jobs.result.progressLabel
								}
							/>
							{pct !== null && (
								<span className="text-sm font-semibold tabular-nums">
									{copy.jobs.result.percent(pct)}
								</span>
							)}
						</div>
						{!scanned && (
							<p className="text-sm text-muted-foreground">
								{copy.jobs.detail.scanningNote}
							</p>
						)}
						<dl
							aria-live="off"
							className="grid grid-cols-2 gap-4 xl:grid-cols-5"
						>
							<Metric
								label={copy.jobs.metrics.files}
								value={formatCount(progress?.files_done ?? 0)}
								sub={
									scanned
										? copy.jobs.metrics.of(
												formatCount(progress?.files_total ?? 0),
											)
										: undefined
								}
							/>
							<Metric
								label={copy.jobs.metrics.data}
								value={formatBytes(progress?.bytes_done ?? 0)}
								sub={
									scanned
										? copy.jobs.metrics.of(
												formatBytes(progress?.bytes_total ?? 0),
											)
										: undefined
								}
							/>
							{restore ? (
								<Metric
									label={copy.jobs.metrics.readFromStorage}
									value={formatBytes(progress?.bytes_downloaded ?? 0)}
									sub={copy.jobs.metrics.readFromStorageSub}
								/>
							) : (
								<Metric
									label={copy.jobs.metrics.uploaded}
									value={formatBytes(progress?.bytes_uploaded ?? 0)}
									sub={copy.jobs.metrics.uploadedSub}
								/>
							)}
							{limit ? (
								<Metric
									label={copy.jobs.metrics.speed}
									value={formatRate(uploadSpeed)}
									sub={copy.jobs.metrics.speedLimit(formatRate(limit))}
									badge={
										uploadSpeed >= THROTTLED_AT * limit && (
											<Tooltip>
												<TooltipTrigger asChild>
													<Badge variant="outline" tabIndex={0}>
														<Gauge aria-hidden />
														{copy.jobs.metrics.throttled}
													</Badge>
												</TooltipTrigger>
												<TooltipContent>
													{copy.jobs.metrics.throttledHint}
												</TooltipContent>
											</Tooltip>
										)
									}
								/>
							) : (
								<Metric
									label={copy.jobs.metrics.speed}
									value={formatRate(progress?.speed_bps ?? 0)}
									sub={copy.jobs.metrics.speedSub}
								/>
							)}
							<Metric
								label={copy.jobs.metrics.timeLeft}
								value={
									scanned ? formatTimeLeft(progress?.eta_s) : copy.common.dash
								}
							/>
						</dl>
					</>
				)}
			</CardContent>
		</Card>
	);
}
