import { createFileRoute, Link } from "@tanstack/react-router";
import { TriangleAlert } from "lucide-react";
import { useAgentPolicy, useGetRepository } from "@/api/gen/endpoints";
import { agentKeyWarning } from "@/components/capability-checklist";
import { PolicyPanel } from "@/components/policy-panel";
import { AgentKeyBadge } from "@/components/repos/agent-key-badge";
import { ReplaceAgentKeyDialog } from "@/components/repos/replace-agent-key-dialog";
import { StorageChecksCard } from "@/components/repos/storage-checks-card";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
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
	Card,
	CardContent,
	CardDescription,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { hostCount } from "@/lib/plural";

export const Route = createFileRoute("/_app/settings/repositories/$id")({
	component: RepositoryPage,
});

function RepositoryPage() {
	const { id } = Route.useParams();
	const query = useGetRepository(id);
	const repository = query.data;
	const options = repository?.location.options;
	const bucket = options?.bucket;
	const isS3 = repository?.backend === "s3";
	const keyWarning = repository && agentKeyWarning(repository);

	const policy = useAgentPolicy(
		{ bucket: bucket ?? "", prefix: options?.prefix },
		{ query: { enabled: isS3 && !!bucket } },
	);

	return (
		<div className="flex flex-col gap-6">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>{copy.repos.breadcrumbSettings}</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/settings/repositories">{copy.repos.title}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>
							{repository?.name ?? copy.common.dash}
						</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>

			{query.isError && !repository && (
				<Alert variant="destructive">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.repos.loadFailed}
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

			{query.isPending && (
				<div className="flex flex-col gap-4">
					<Skeleton className="h-8 w-48" />
					<Skeleton className="h-64 w-full" />
				</div>
			)}

			{repository && (
				<>
					<header className="flex items-center gap-3">
						<h1 className="min-w-0 text-2xl font-semibold break-all">
							{repository.name}
						</h1>
						<AgentKeyBadge state={repository.agent_key_state} />
					</header>

					{keyWarning && (
						<Alert className="text-warning">
							<TriangleAlert aria-hidden />
							<AlertTitle>{keyWarning.heading}</AlertTitle>
							<AlertDescription>{keyWarning.body}</AlertDescription>
						</Alert>
					)}

					<StorageChecksCard
						repositoryId={repository.id}
						backend={repository.backend}
						checks={repository.checks}
						agentKeyState={repository.agent_key_state}
						checkedAt={repository.checked_at}
					/>

					<Card>
						<CardHeader>
							<CardTitle>
								<h2>{copy.repos.agentKeyTitle}</h2>
							</CardTitle>
							<CardDescription>
								{isS3 ? copy.backend.twoKeys : copy.repos.localAgentKey}
							</CardDescription>
						</CardHeader>
						{isS3 && (
							<CardContent className="flex flex-col gap-4">
								<PolicyPanel
									iamJson={policy.data?.iam_json}
									minioCommands={policy.data?.minio_commands}
									loading={policy.isLoading}
								/>
								<ReplaceAgentKeyDialog
									repositoryId={repository.id}
									name={repository.name}
								/>
							</CardContent>
						)}
					</Card>

					<Card>
						<CardHeader>
							<CardTitle>
								<h2>{copy.repos.hostsTitle}</h2>
							</CardTitle>
							<CardDescription>
								{hostCount(repository.hosts_count)}
							</CardDescription>
						</CardHeader>
						<CardContent>
							<Button asChild variant="secondary">
								<Link to="/hosts">{copy.wizard.openHosts}</Link>
							</Button>
						</CardContent>
					</Card>
				</>
			)}
		</div>
	);
}
