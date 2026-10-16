import { createFileRoute, Link } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useGetJob } from "@/api/gen/endpoints";
import type { JobDetail } from "@/api/gen/model";
import {
	CancelJobDialog,
	useRerunAction,
} from "@/components/jobs/cancel-job-dialog";
import { JobCards } from "@/components/jobs/job-cards";
import { JobLog, LOG_ANCHOR, type LogFilter } from "@/components/jobs/job-log";
import { JobProgress, phaseLabel } from "@/components/jobs/job-progress";
import { waitReason } from "@/components/jobs/jobs-table";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
	Breadcrumb,
	BreadcrumbItem,
	BreadcrumbLink,
	BreadcrumbList,
	BreadcrumbPage,
	BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { shortId } from "@/lib/snapshot";

export const Route = createFileRoute("/_app/jobs/$id")({
	component: JobPage,
});

const POLL_MS = 2_000;

const isActive = (state?: string) => state === "queued" || state === "running";

function announcement(job: JobDetail) {
	const status = copy.jobs.detail.announce[job.state];
	return job.state === "running" ? `${status}. ${phaseLabel(job)}` : status;
}

function NotFound() {
	return (
		<Empty>
			<EmptyHeader>
				<EmptyTitle>
					<h1 data-page-title tabIndex={-1} className="text-xl outline-none">
						{copy.jobs.detail.notFoundHeading}
					</h1>
				</EmptyTitle>
				<EmptyDescription>{copy.jobs.detail.notFoundBody}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					<Link to="/jobs">{copy.jobs.detail.backToJobs}</Link>
				</Button>
			</EmptyContent>
		</Empty>
	);
}

function Notice({ children }: { children: string }) {
	return (
		<Alert role="note">
			<AlertDescription className="text-current">{children}</AlertDescription>
		</Alert>
	);
}

function JobNotices({ job }: { job: JobDetail }) {
	if (job.kind === "download") return null;
	const restore = job.kind === "restore";
	const d = copy.jobs.detail;
	const q = copy.jobs.queueReason;
	if (job.state === "running" && !job.host_online) {
		return (
			<Notice>
				{restore
					? d.offlineWhileRestoring(job.hostname)
					: d.offlineWhileRunning(job.hostname)}
			</Notice>
		);
	}
	if (job.state !== "queued") return null;
	const reason = waitReason(job);
	if (reason === undefined) return null;
	const offline = reason === "host_offline";
	if (restore) {
		return (
			<Notice>
				{offline ? q.offlineLong(job.hostname) : q.busyJobLong(job.hostname)}
			</Notice>
		);
	}
	return (
		<Notice>
			{offline
				? `${q.offlineShort(job.hostname)} ${q.dropAfter}`
				: q.busyLong(job.hostname)}
		</Notice>
	);
}

function JobSubline({ job }: { job: JobDetail }) {
	const info = job.restore ?? job.download;
	if (job.kind === "restore" || job.kind === "download") {
		if (!info) return null;
		const host = job.restore?.source_host_key ?? job.host_id;
		return (
			<>
				{job.kind === "restore"
					? copy.jobs.detail.restoreOf
					: copy.jobs.detail.downloadOf}{" "}
				<Link
					to="/restore"
					search={{ host, snapshot: info.snapshot_id }}
					className="rounded-sm text-primary-strong"
				>
					<code className="font-mono">{shortId(info.snapshot_id)}</code>
				</Link>
			</>
		);
	}
	if (job.source === "cli" || !job.policy_name) return copy.jobs.runFromCli;
	if (!job.policy_exists) return copy.jobs.policyLine(job.policy_name);
	return (
		<Link
			to="/policies/$name"
			params={{ name: job.policy_name }}
			className="rounded-sm text-primary-strong"
		>
			{copy.jobs.policyLine(job.policy_name)}
		</Link>
	);
}

function JobPage() {
	const { id } = Route.useParams();
	const query = useGetJob(id, {
		query: {
			refetchInterval: (q) => (isActive(q.state.data?.state) ? POLL_MS : false),
			refetchIntervalInBackground: false,
		},
	});
	const job = query.data;
	const notFound =
		query.error instanceof ApiError && query.error.status === 404;
	const [cancelling, setCancelling] = useState(false);
	const rerun = useRerunAction({ open: true });

	const state = job?.state;
	const previous = useRef(state);
	const { refetch } = query;
	useEffect(() => {
		if (isActive(previous.current) && state && !isActive(state)) {
			void refetch();
		}
		previous.current = state;
	}, [state, refetch]);

	const [logFilter, setLogFilter] = useState<LogFilter>("all");
	const showWarnings = () => {
		setLogFilter("warnings");
		document.getElementById(LOG_ANCHOR)?.scrollIntoView({ block: "start" });
	};

	return (
		<div className="flex flex-col gap-6 pb-8">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/jobs">{copy.jobs.breadcrumbRoot}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>{job?.hostname ?? copy.common.dash}</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>

			{query.isError && !job && !notFound && (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.jobs.detail.loadFailed}
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => query.refetch()}
						>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}

			{notFound && !job && <NotFound />}

			{query.isPending && (
				<div className="flex flex-col gap-6">
					<div className="flex flex-col gap-2">
						<Skeleton className="h-8 w-64" />
						<Skeleton className="h-4 w-40" />
					</div>
					<Skeleton className="h-48 w-full" />
				</div>
			)}

			{job && (
				<>
					<header className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-start">
						<div className="flex min-w-0 flex-col gap-1">
							<div className="flex flex-wrap items-center gap-2">
								<h1
									data-page-title
									className="min-w-0 text-2xl font-semibold wrap-anywhere"
								>
									{job.hostname}
								</h1>
								<StatusBadge status={job.state} />
							</div>
							<p className="text-sm text-muted-foreground wrap-anywhere">
								<JobSubline job={job} />
							</p>
						</div>
						<div className="flex gap-2">
							{isActive(job.state) && job.kind !== "download" && (
								<Button
									variant="outline"
									className="text-destructive"
									onClick={() => setCancelling(true)}
								>
									{copy.jobs.cancelJob}
								</Button>
							)}
							{!isActive(job.state) &&
								job.kind === "backup" &&
								job.source !== "cli" &&
								job.policy_exists && (
									<Button
										variant="outline"
										disabled={rerun.pending}
										onClick={() => rerun.run(job)}
									>
										{rerun.pending && <Spinner />}
										{copy.jobs.runAgain}
									</Button>
								)}
						</div>
					</header>

					<div role="status" className="sr-only">
						{announcement(job)}
					</div>

					<JobNotices job={job} />
					{isActive(job.state) && (
						<JobProgress job={job} onShowWarnings={showWarnings} />
					)}
					<JobCards job={job} />
					{job.kind !== "download" && (
						<JobLog
							key={job.id}
							jobId={job.id}
							active={isActive(job.state)}
							purged={job.logs_purged}
							filter={logFilter}
							onFilterChange={setLogFilter}
						/>
					)}
				</>
			)}

			<CancelJobDialog
				job={cancelling ? job : undefined}
				onClose={() => setCancelling(false)}
			/>
		</div>
	);
}
