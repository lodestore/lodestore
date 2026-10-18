import { Link } from "@tanstack/react-router";
import { useListHostPolicies } from "@/api/gen/endpoints";
import type { HostDetail } from "@/api/gen/model";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";

const REFRESH_MS = 10_000;

export function HostPolicies({ host }: { host: HostDetail }) {
	const revoked = host.status === "revoked";
	const query = useListHostPolicies(host.id, {
		query: {
			enabled: !revoked,
			refetchInterval: REFRESH_MS,
			refetchIntervalInBackground: false,
		},
	});
	if (revoked) return null;
	const policies = query.data;

	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2>{copy.hostBackup.policiesCard}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-2">
				{policies === undefined && query.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription className="flex items-center justify-between gap-4">
							{copy.policies.loadFailed}
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
				{policies === undefined && query.isPending && (
					<div aria-hidden className="flex flex-col gap-2">
						<Skeleton className="h-6 w-full" />
						<Skeleton className="h-6 w-full" />
					</div>
				)}
				{policies?.length === 0 && (
					<>
						<p className="text-sm">{copy.hostBackup.noPolicy}</p>
						<Button asChild variant="link" className="h-auto self-start p-0">
							<Link to="/policies/new">{copy.hostBackup.createPolicy}</Link>
						</Button>
					</>
				)}
				{policies && policies.length > 0 && (
					<ul className="flex flex-col gap-2">
						{policies.map(({ name, last_job, next_run }) => (
							<li
								key={name}
								className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm"
							>
								<Link
									to="/policies/$name"
									params={{ name }}
									className="min-w-0 truncate font-semibold text-primary-strong underline-offset-4 hover:underline"
								>
									{name}
								</Link>
								{last_job ? (
									<>
										<StatusBadge status={last_job.state} />
										<RelativeTime
											value={
												new Date(last_job.finished_at ?? last_job.queued_at)
											}
										/>
									</>
								) : (
									<span className="text-muted-foreground">
										{copy.policies.neverRun}
									</span>
								)}
								{next_run != null && (
									<span className="text-muted-foreground">
										{copy.policies.next}{" "}
										<RelativeTime value={new Date(next_run)} numeric="always" />
									</span>
								)}
							</li>
						))}
					</ul>
				)}
			</CardContent>
		</Card>
	);
}
