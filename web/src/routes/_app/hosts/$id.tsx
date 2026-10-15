import { createFileRoute, Link } from "@tanstack/react-router";
import { Camera, ListChecks } from "lucide-react";
import { useState } from "react";
import { z } from "zod";
import { ApiError } from "@/api/fetcher";
import { useGetHost } from "@/api/gen/endpoints";
import type { HostDetail } from "@/api/gen/model";
import { EditLabelsDialog } from "@/components/hosts/edit-labels-dialog";
import {
	HostOverview,
	HostOverviewSkeleton,
} from "@/components/hosts/host-overview";
import { RevokeHostDialog } from "@/components/hosts/revoke-host-dialog";
import { PreviewPage } from "@/components/preview-page";
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
					<header className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-start">
						<div className="flex min-w-0 flex-col gap-1">
							<div className="flex flex-wrap items-center gap-2">
								<h1
									data-page-title
									className="min-w-0 text-2xl font-semibold break-words"
								>
									{host.hostname}
								</h1>
								<StatusBadge status={host.status} />
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
							<div className="flex gap-2">
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
							<PreviewPage
								embedded
								icon={Camera}
								illustration="preview-host-tabs"
								{...copy.preview.hostSnapshots}
							/>
						</TabsContent>
						<TabsContent value="jobs">
							<PreviewPage
								embedded
								icon={ListChecks}
								illustration="preview-jobs"
								{...copy.preview.hostJobs}
							/>
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
