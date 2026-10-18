import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import { useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import {
	useGetPolicy,
	useListJobs,
	useListRepositories,
} from "@/api/gen/endpoints";
import {
	CancelJobDialog,
	useRerunAction,
} from "@/components/jobs/cancel-job-dialog";
import {
	JobActions,
	JobResult,
	type JobRow,
	jobsRefetchInterval,
	toJobRow,
} from "@/components/jobs/jobs-table";
import { DeletePolicyDialog } from "@/components/policies/delete-policy-dialog";
import { PolicyEditor } from "@/components/policies/policy-editor";
import { RunNowDialog } from "@/components/policies/run-now-dialog";
import { RelativeTime } from "@/components/relative-time";
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
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { hostCount } from "@/lib/plural";

export const Route = createFileRoute("/_app/policies/$name")({
	component: PolicyPage,
});

function NotFound() {
	return (
		<Empty>
			<EmptyHeader>
				<EmptyTitle>
					<h1 data-page-title tabIndex={-1} className="text-xl outline-none">
						{copy.policies.notFoundHeading}
					</h1>
				</EmptyTitle>
				<EmptyDescription>{copy.policies.notFoundBody}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					<Link to="/policies">{copy.policies.backToPolicies}</Link>
				</Button>
			</EmptyContent>
		</Empty>
	);
}

const RECENT = 5;

function RecentJob({
	job,
	onOpen,
	onCancel,
	onRerun,
}: {
	job: JobRow;
	onOpen: (job: JobRow) => void;
	onCancel: (job: JobRow) => void;
	onRerun: (job: JobRow) => void;
}) {
	const at =
		job.state === "queued" ? job.queued_at : (job.started_at ?? job.queued_at);
	return (
		<li data-job={job.id} className="flex items-start gap-2 py-4">
			<div className="flex min-w-0 flex-1 flex-col gap-2">
				<div className="flex min-w-0 items-center gap-2">
					<Link
						to="/jobs/$id"
						params={{ id: job.id }}
						className="min-w-0 truncate rounded-sm font-semibold"
					>
						{job.hostname}
					</Link>
					<span className="shrink-0 text-sm text-muted-foreground">
						<RelativeTime value={new Date(at)} />
					</span>
				</div>
				<div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
					<StatusBadge status={job.state} />
					{job.state !== "cancelled" && <JobResult job={job} />}
				</div>
			</div>
			<JobActions
				job={job}
				onOpen={onOpen}
				onCancel={onCancel}
				onRerun={onRerun}
			/>
		</li>
	);
}

function RecentJobs({ name }: { name: string }) {
	const jobs = useListJobs(
		{ policy: name, page: 1 },
		{
			query: {
				refetchInterval: jobsRefetchInterval,
				refetchIntervalInBackground: false,
			},
		},
	);
	const items = (jobs.data?.items ?? []).slice(0, RECENT).map(toJobRow);
	const rerun = useRerunAction();
	const [cancelling, setCancelling] = useState<JobRow>();
	const navigate = useNavigate();
	const openJob = (job: JobRow) =>
		void navigate({ to: "/jobs/$id", params: { id: job.id } });

	return (
		<Card>
			<CardHeader className="flex items-center justify-between">
				<CardTitle>
					<h2>{copy.policies.editor.recentJobs}</h2>
				</CardTitle>
				<Button asChild variant="link" className="h-auto p-0 text-sm">
					<Link to="/jobs" search={{ policy: name } as never}>
						{copy.jobs.viewAll}
					</Link>
				</Button>
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				{jobs.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription className="flex flex-wrap items-center justify-between gap-2">
							{copy.jobs.loadFailed}
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={() => void jobs.refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				)}
				{jobs.isSuccess && items.length === 0 && (
					<p className="text-sm text-muted-foreground">
						{copy.policies.neverRun}
					</p>
				)}
				{jobs.isPending && (
					<div className="flex flex-col gap-4" aria-hidden>
						{[0, 1, 2].map((i) => (
							<Skeleton key={i} className="h-14 w-full" />
						))}
					</div>
				)}
				{items.length > 0 && (
					<ul className="-my-4 divide-y">
						{items.map((job) => (
							<RecentJob
								key={job.id}
								job={job}
								onOpen={openJob}
								onCancel={setCancelling}
								onRerun={rerun.run}
							/>
						))}
					</ul>
				)}
				<CancelJobDialog
					job={cancelling}
					onClose={() => setCancelling(undefined)}
				/>
			</CardContent>
		</Card>
	);
}

function PolicyPage() {
	const { name } = Route.useParams();
	const query = useGetPolicy(name);
	const repositories = useListRepositories();
	const [changed, setChanged] = useState(false);
	const navigate = useNavigate();
	const menuOrigin = useRef<HTMLElement | null>(null);
	const leaving = useRef(false);
	const [running, setRunning] = useState<string>();
	const [deleting, setDeleting] = useState<string>();
	const detail = query.data;
	const notFound =
		query.error instanceof ApiError && query.error.status === 404;

	return (
		<div className="flex flex-col gap-6 pb-8">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/policies">{copy.policies.title}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>{name}</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>

			{notFound && <NotFound />}
			{query.isError && !detail && !notFound && (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.policies.loadFailed}
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => void query.refetch()}
						>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}
			{query.isPending && <Skeleton className="h-96 w-full" />}

			{detail && repositories.data && (
				<>
					<header className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-start">
						<div className="flex min-w-0 flex-col gap-1">
							<h1
								data-page-title
								className="min-w-0 text-2xl font-semibold break-words"
							>
								{detail.name}
							</h1>
							<p className="text-sm text-muted-foreground">
								{copy.policies.subtitle(
									detail.repository_name,
									hostCount(detail.matching.hosts.length),
								)}
							</p>
						</div>
						<div className="flex gap-2">
							<Button
								variant={changed ? "outline" : "default"}
								onClick={() => setRunning(detail.name)}
							>
								{copy.policies.runNow}
							</Button>
							<DropdownMenu modal={false}>
								<DropdownMenuTrigger asChild>
									<Button
										variant="outline"
										size="icon"
										aria-label={copy.policies.moreActions}
										onClick={(e) => {
											menuOrigin.current = e.currentTarget;
										}}
									>
										<MoreHorizontal aria-hidden />
									</Button>
								</DropdownMenuTrigger>
								<DropdownMenuContent align="end">
									<DropdownMenuItem
										variant="destructive"
										onSelect={() => setDeleting(detail.name)}
									>
										{copy.policies.delete}
									</DropdownMenuItem>
								</DropdownMenuContent>
							</DropdownMenu>
						</div>
					</header>
					<PolicyEditor
						key={detail.name}
						detail={detail}
						leaving={leaving}
						repositories={repositories.data}
						onChangedChange={setChanged}
					>
						<RecentJobs name={detail.name} />
					</PolicyEditor>
					<RunNowDialog name={running} onClose={() => setRunning(undefined)} />
					<DeletePolicyDialog
						name={deleting}
						managedBy={detail.managed_by}
						onClose={() => setDeleting(undefined)}
						onDeleted={() => {
							leaving.current = true;
							void navigate({ to: "/policies" });
						}}
						onCloseAutoFocus={(event) => {
							event.preventDefault();
							menuOrigin.current?.focus();
						}}
					/>
				</>
			)}
		</div>
	);
}
