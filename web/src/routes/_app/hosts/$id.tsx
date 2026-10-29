import { createFileRoute, Link } from "@tanstack/react-router";
import { useState } from "react";
import { z } from "zod";
import { ApiError } from "@/api/fetcher";
import { useGetHost, useListJobs } from "@/api/gen/endpoints";
import type { HostDetail } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { EditLabelsDialog } from "@/components/hosts/edit-labels-dialog";
import {
	HostOverview,
	HostOverviewSkeleton,
	useHostHealth,
} from "@/components/hosts/host-overview";
import { RevokeHostDialog } from "@/components/hosts/revoke-host-dialog";
import { RunBackupButton } from "@/components/hosts/run-backup-button";
import { SnapshotsTable } from "@/components/hosts/snapshots-table";
import {
	CancelJobDialog,
	useRerunAction,
} from "@/components/jobs/cancel-job-dialog";
import { type JobRow, JobsTable, toJobRow } from "@/components/jobs/jobs-table";
import { HostStateBadge } from "@/components/overview/host-state";
import { SilenceMenu } from "@/components/overview/silence-menu";
import { formatRelative } from "@/components/relative-time";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";

const TABS = ["overview", "snapshots", "jobs"] as const;
const JOBS_REFRESH_MS = 5_000;

export const Route = createFileRoute("/_app/hosts/$id")({
	validateSearch: z.object({ tab: z.enum(TABS).optional().catch(undefined) }),
	component: HostPage,
});

function NotFound() {
	return (
		<Empty>
			<EmptyHeader>
				<EmptyTitle>
					<h1 data-page-title tabIndex={-1} className="text-xl outline-none">
						{copy.hostPage.notFound.heading}
					</h1>
				</EmptyTitle>
				<EmptyDescription>{copy.hostPage.notFound.body}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					<Link to="/hosts">{copy.hostPage.notFound.back}</Link>
				</Button>
			</EmptyContent>
		</Empty>
	);
}

function HostJobs({ host }: { host: HostDetail }) {
	const query = useListJobs(
		{ host_id: host.id },
		{
			query: {
				refetchInterval: JOBS_REFRESH_MS,
				refetchIntervalInBackground: false,
			},
		},
	);
	const rerun = useRerunAction({ open: true });
	const [cancelling, setCancelling] = useState<JobRow>();
	const jobs = query.data?.items.map(toJobRow);

	if (jobs === undefined && query.isError) {
		return (
			<Alert variant="destructive" role="alert" className="mt-4">
				<AlertDescription className="flex items-center justify-between gap-4">
					{copy.jobs.loadFailed}
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
		);
	}
	if (jobs?.length === 0) {
		return (
			<Empty className="py-24">
				<EmptyHeader>
					<Illustration
						name="preview-host-tabs"
						width={320}
						height={216}
						className="h-auto w-80 max-w-full"
					/>
					<EmptyTitle>{copy.hostBackup.jobsEmptyHeading}</EmptyTitle>
					<EmptyDescription>{copy.hostBackup.jobsEmptyBody}</EmptyDescription>
				</EmptyHeader>
			</Empty>
		);
	}
	return (
		<div className="flex flex-col gap-4 pt-4">
			<JobsTable
				jobs={jobs ?? []}
				loading={jobs === undefined}
				hide={["host"]}
				onCancel={setCancelling}
				onRerun={rerun.run}
			/>
			<CancelJobDialog
				job={cancelling}
				onClose={() => setCancelling(undefined)}
			/>
			<Link
				to="/jobs"
				search={{ host: host.hostname }}
				className="self-start text-sm font-semibold text-primary-strong underline-offset-4 hover:underline"
			>
				{copy.hostBackup.openInJobs}
			</Link>
		</div>
	);
}

function HostPage() {
	const { id } = Route.useParams();
	const { tab = "overview" } = Route.useSearch();
	const navigate = Route.useNavigate();
	const query = useGetHost(id);
	const host = query.data;
	const notFound =
		query.error instanceof ApiError && query.error.status === 404;
	const [editing, setEditing] = useState<HostDetail>();
	const [revoking, setRevoking] = useState<HostDetail>();
	const revoked = host?.status === "revoked";
	const health = useHostHealth(id).data;

	return (
		<div className="flex flex-col gap-6 pb-8">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/hosts">{copy.hosts.title}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>
							{host?.hostname ?? copy.common.dash}
						</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>

			{query.isError && !host && !notFound && (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.hostPage.loadFailed}
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

			{notFound && <NotFound />}

			{query.isPending && (
				<div className="flex flex-col gap-6">
					<div className="flex flex-col gap-2">
						<Skeleton className="h-8 w-64" />
						<Skeleton className="h-4 w-40" />
					</div>
					<HostOverviewSkeleton />
				</div>
			)}

			{host && (
				<>
					<header className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_auto] xl:items-start">
						<div className="flex min-w-0 flex-col gap-1">
							<div className="flex flex-wrap items-center gap-2">
								<h1
									data-page-title
									className="min-w-0 text-2xl font-semibold break-words"
								>
									{host.hostname}
								</h1>
								{health && <HostStateBadge state={health.state} />}
								<StatusBadge status={host.status}>
									{host.status === "revoked"
										? undefined
										: copy.hostPage.agent[host.status]}
								</StatusBadge>
							</div>
							{host.last_seen_at != null && (
								<time
									dateTime={new Date(host.last_seen_at).toISOString()}
									className="text-sm text-muted-foreground"
								>
									{copy.hosts.lastSeen(
										formatRelative(new Date(host.last_seen_at), new Date()),
									)}
								</time>
							)}
						</div>
						{!revoked && (
							<div className="flex flex-wrap gap-2">
								<RunBackupButton host={host} />
								{health && (
									<SilenceMenu
										hostId={host.id}
										hostname={host.hostname}
										silenced={health.silenced}
									/>
								)}
								<Button variant="outline" onClick={() => setEditing(host)}>
									{copy.hostPage.editLabels}
								</Button>
								<Button
									variant="outline"
									className="text-destructive"
									onClick={() => setRevoking(host)}
								>
									{copy.hostPage.revoke}
								</Button>
							</div>
						)}
					</header>
					{revoked && (
						<Alert>
							<AlertDescription>{copy.hostPage.revokedBanner}</AlertDescription>
						</Alert>
					)}
					<Tabs
						value={tab}
						onValueChange={(value) =>
							void navigate({
								search: { tab: value as (typeof TABS)[number] },
								replace: true,
							})
						}
					>
						<TabsList variant="line">
							{TABS.map((name) => (
								<TabsTrigger
									key={name}
									value={name}
									className="after:bg-primary-strong"
								>
									{copy.hostPage.tabs[name]}
								</TabsTrigger>
							))}
						</TabsList>
						<TabsContent value="overview" className="pt-4">
							<HostOverview host={host} />
						</TabsContent>
						<TabsContent value="snapshots">
							<SnapshotsTable key={host.id} host={host} />
						</TabsContent>
						<TabsContent value="jobs">
							<HostJobs host={host} />
						</TabsContent>
					</Tabs>
				</>
			)}

			<EditLabelsDialog host={editing} onClose={() => setEditing(undefined)} />
			<RevokeHostDialog
				host={revoking}
				onClose={() => setRevoking(undefined)}
			/>
		</div>
	);
}
