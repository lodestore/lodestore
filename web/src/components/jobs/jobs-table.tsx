import { Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import type { JobSummary } from "@/api/gen/model";
import { formatRelative } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Progress } from "@/components/ui/progress";
import { Skeleton } from "@/components/ui/skeleton";
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
import {
	formatBytes,
	formatCount,
	formatDuration,
	percent,
} from "@/lib/format";
import { warningCount } from "@/lib/plural";
import { cn } from "@/lib/utils";

export type JobState =
	| "queued"
	| "running"
	| "succeeded"
	| "warnings"
	| "failed"
	| "cancelled"
	| "interrupted"
	| "not_started";

export type JobRow = {
	id: string;
	host_id: string;
	hostname: string;
	policy_name?: string | null;
	policy_exists: boolean;
	source: "manual" | "cli";
	stdin_name?: string | null;
	state: JobState;
	phase?: string | null;
	queued_at: number;
	started_at?: number | null;
	finished_at?: number | null;
	queue_reason?: "host_offline" | "host_busy" | null;
	host_online: boolean;
	progress?: {
		files_done: number;
		files_total: number;
		bytes_done: number;
		bytes_total: number;
		bytes_uploaded: number;
		scan_done: boolean;
		speed_bps: number;
		eta_s?: number | null;
		updated_at: number;
	} | null;
	stats?: {
		bytes_total: number;
		files_new: number;
		files_changed: number;
		files_unchanged: number;
	} | null;
	incomplete: boolean;
	warnings_total: number;
	errors_total: number;
	error_code?: string | null;
	error_message?: string | null;
};

export const toJobRow = ({ progress, stats, ...job }: JobSummary): JobRow => ({
	...job,
	progress: progress && {
		files_done: 0,
		files_total: 0,
		bytes_done: 0,
		bytes_total: 0,
		bytes_uploaded: 0,
		scan_done: false,
		speed_bps: 0,
		updated_at: 0,
		...progress,
	},
	stats: stats && {
		bytes_total: 0,
		files_new: 0,
		files_changed: 0,
		files_unchanged: 0,
		...stats,
	},
});

const ACTIVE_REFRESH_MS = 3_000;
const IDLE_REFRESH_MS = 15_000;

export const jobsRefetchInterval = (query: {
	state: { data?: { items: { state: string }[] } };
}) =>
	query.state.data?.items.some(
		(j) => j.state === "queued" || j.state === "running",
	)
		? ACTIVE_REFRESH_MS
		: IDLE_REFRESH_MS;

const SHOW_STARTED = "hidden @min-[1024px]:table-cell";
const SHOW_DURATION = "hidden @min-[1280px]:table-cell";
const SHOW_STATUS = "hidden @min-[1024px]:table-cell";

const isActive = (job: JobRow) =>
	job.state === "queued" || job.state === "running";

function Truncated({
	text,
	className,
	children,
}: {
	text: string;
	className?: string;
	children?: React.ReactNode;
}) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span className={cn("block truncate", className)}>
					{children ?? text}
				</span>
			</TooltipTrigger>
			<TooltipContent>{text}</TooltipContent>
		</Tooltip>
	);
}

export function JobResult({ job }: { job: JobRow }) {
	const { progress, stats } = job;
	switch (job.state) {
		case "running":
			if (progress?.scan_done) {
				return (
					<div className="flex items-center gap-2">
						<Progress
							value={percent(progress)}
							aria-label={copy.jobs.result.progressLabel}
							className="w-32 shrink-0"
						/>
						<span className="text-sm">
							{copy.jobs.result.percent(percent(progress))}
						</span>
					</div>
				);
			}
			return (
				<span className="text-sm">
					{copy.jobs.result.scanning(
						formatCount(
							Math.max(progress?.files_done ?? 0, progress?.files_total ?? 0),
						),
					)}
				</span>
			);
		case "queued": {
			const offline = job.queue_reason
				? job.queue_reason === "host_offline"
				: !job.host_online;
			return (
				<span className="text-sm text-muted-foreground">
					{offline ? copy.jobs.queueReason.offline : copy.jobs.queueReason.busy}
				</span>
			);
		}
		case "succeeded":
			if (!stats) return <span>{copy.common.dash}</span>;
			return (
				<span className="text-sm">
					{copy.jobs.result.succeeded(
						formatBytes(stats.bytes_total),
						formatCount(
							stats.files_new + stats.files_changed + stats.files_unchanged,
						),
					)}
				</span>
			);
		case "warnings":
			return (
				<span className="text-sm">
					{copy.jobs.result.warnings(
						formatBytes(stats?.bytes_total ?? 0),
						warningCount(job.warnings_total),
					)}
				</span>
			);
		case "failed": {
			const reason =
				job.error_message?.split("\n")[0].trim() || copy.jobs.statusHint.failed;
			return <Truncated text={reason} className="max-w-64 text-sm" />;
		}
		default:
			return (
				<span className="text-sm text-muted-foreground">
					{copy.jobs.statusHint[job.state]}
				</span>
			);
	}
}

function Started({ job }: { job: JobRow }) {
	const queued = job.state === "queued";
	const at = new Date(
		queued ? job.queued_at : (job.started_at ?? job.queued_at),
	);
	const relative = formatRelative(at, new Date());
	return (
		<Link
			to="/jobs/$id"
			params={{ id: job.id }}
			className="rounded-sm text-sm"
			title={at.toLocaleString("en", {
				dateStyle: "medium",
				timeStyle: "medium",
			})}
		>
			<time dateTime={at.toISOString()}>
				{queued ? copy.jobs.queued(relative) : relative}
			</time>
		</Link>
	);
}

function Duration({ job }: { job: JobRow }) {
	if (job.state === "running" && job.started_at != null) {
		return (
			<span className="text-sm">
				{copy.jobs.running(formatDuration(Date.now() - job.started_at))}
			</span>
		);
	}
	if (job.started_at != null && job.finished_at != null) {
		return (
			<span className="text-sm">
				{formatDuration(job.finished_at - job.started_at)}
			</span>
		);
	}
	return <span>{copy.common.dash}</span>;
}

function PolicyLine({ job }: { job: JobRow }) {
	if (job.source === "cli") {
		return (
			<Badge variant="outline" title={copy.jobs.runFromCli}>
				{copy.jobs.cliBadge}
			</Badge>
		);
	}
	if (!job.policy_name) return null;
	return (
		<div className="max-w-48 truncate text-sm text-muted-foreground">
			{copy.jobs.policyLine(job.policy_name)}
		</div>
	);
}

function SkeletonRows({ showHost }: { showHost: boolean }) {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-16">
			<TableCell className={cn("px-4", showHost && SHOW_STATUS)}>
				<Skeleton className="h-5 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-40" />
				<Skeleton className="mt-2 h-3 w-16" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_STARTED)}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_DURATION)}>
				<Skeleton className="h-4 w-16" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-32" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="size-10" />
			</TableCell>
		</TableRow>
	));
}

export function JobActions({
	job,
	onOpen,
	onCancel,
	onRerun,
}: {
	job: JobRow;
	onOpen: (job: JobRow) => void;
	onCancel?: (job: JobRow) => void;
	onRerun?: (job: JobRow) => void;
}) {
	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					data-job-actions={job.id}
					aria-label={copy.jobs.actionsFor(job.hostname)}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem onSelect={() => onOpen(job)}>
					{copy.jobs.viewJob}
				</DropdownMenuItem>
				{isActive(job) && (
					<DropdownMenuItem
						variant="destructive"
						onSelect={() => onCancel?.(job)}
					>
						{copy.jobs.cancelJob}
					</DropdownMenuItem>
				)}
				{!isActive(job) && job.source !== "cli" && job.policy_exists && (
					<DropdownMenuItem onSelect={() => onRerun?.(job)}>
						{copy.jobs.runAgain}
					</DropdownMenuItem>
				)}
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

export function JobsTable({
	jobs,
	loading = false,
	hide = [],
	onCancel,
	onRerun,
}: {
	jobs: JobRow[];
	loading?: boolean;
	hide?: Array<"host" | "policy">;
	onCancel?: (job: JobRow) => void;
	onRerun?: (job: JobRow) => void;
}) {
	const navigate = useNavigate();
	const showHost = !hide.includes("host");
	const showPolicy = !hide.includes("policy");
	const openJob = (job: JobRow) =>
		void navigate({ to: "/jobs/$id", params: { id: job.id } });

	if (!loading && jobs.length === 0) return null;

	return (
		<div className="@container overflow-hidden rounded-lg border bg-card">
			<Table>
				<TableHeader className="bg-muted">
					<TableRow>
						<TableHead className={cn("px-4", showHost && SHOW_STATUS)}>
							{copy.jobs.columns.status}
						</TableHead>
						<TableHead className="px-4">
							{showHost ? copy.jobs.columns.host : copy.jobs.columns.policy}
						</TableHead>
						<TableHead className={cn("px-4", SHOW_STARTED)}>
							{copy.jobs.columns.started}
						</TableHead>
						<TableHead className={cn("px-4", SHOW_DURATION)}>
							{copy.jobs.columns.duration}
						</TableHead>
						<TableHead className="px-4">{copy.jobs.columns.result}</TableHead>
						<TableHead className="w-16 px-4">
							<span className="sr-only">{copy.jobs.columns.actions}</span>
						</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{loading ? (
						<SkeletonRows showHost={showHost} />
					) : (
						jobs.map((job) => (
							<TableRow
								key={job.id}
								data-job={job.id}
								className="h-16 cursor-pointer"
								onClick={(e) => {
									const target = e.target as HTMLElement;
									if (!target.closest("a,button,[role=menuitem]")) {
										openJob(job);
									}
								}}
							>
								<TableCell className={cn("px-4", showHost && SHOW_STATUS)}>
									<StatusBadge status={job.state} />
								</TableCell>
								<TableCell className="px-4">
									{showHost ? (
										<>
											<Truncated text={job.hostname} className="max-w-52">
												<Link
													to="/hosts/$id"
													params={{ id: job.host_id }}
													className="block max-w-52 truncate rounded-sm text-base font-semibold"
												>
													{job.hostname}
												</Link>
											</Truncated>
											{showPolicy && <PolicyLine job={job} />}
											<div className="@min-[1024px]:hidden">
												<StatusBadge status={job.state} />
											</div>
										</>
									) : (
										showPolicy && <PolicyLine job={job} />
									)}
								</TableCell>
								<TableCell className={cn("px-4", SHOW_STARTED)}>
									<Started job={job} />
								</TableCell>
								<TableCell className={cn("px-4", SHOW_DURATION)}>
									<Duration job={job} />
								</TableCell>
								<TableCell className="px-4">
									<JobResult job={job} />
								</TableCell>
								<TableCell
									className="px-4"
									onClick={(e) => e.stopPropagation()}
								>
									<JobActions
										job={job}
										onOpen={openJob}
										onCancel={onCancel}
										onRerun={onRerun}
									/>
								</TableCell>
							</TableRow>
						))
					)}
				</TableBody>
			</Table>
		</div>
	);
}
