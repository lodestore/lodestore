import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useRecheck,
} from "@/api/gen/endpoints";
import type { CheckDto } from "@/api/gen/model";
import {
	CapabilityChecklist,
	CHECK_IDS,
	type CheckRow,
	toCheckRows,
} from "@/components/capability-checklist";
import { RelativeTime } from "@/components/relative-time";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

export function StorageChecksCard({
	repositoryId,
	backend,
	checks,
	agentKeyState,
	checkedAt,
}: {
	repositoryId: string;
	backend: string;
	checks: CheckDto[];
	agentKeyState: string;
	checkedAt?: number | null;
}) {
	const queryClient = useQueryClient();
	const [error, setError] = useState<string>();
	const recheck = useRecheck({
		mutation: {
			onMutate: () => setError(undefined),
			onError: (e) =>
				setError(copy.errors.submit(copy.repos.checkFailed, e.message)),
			onSuccess: () =>
				Promise.all([
					queryClient.invalidateQueries({
						queryKey: getGetRepositoryQueryKey(repositoryId),
					}),
					queryClient.invalidateQueries({
						queryKey: getListRepositoriesQueryKey(),
					}),
				]),
		},
	});

	const rows: CheckRow[] = recheck.isPending
		? CHECK_IDS.map((id) => ({ id, status: "running" }))
		: toCheckRows({ checks, agent_key_state: agentKeyState }, backend, {
				brief: true,
			});

	return (
		<Card>
			<CardHeader>
				<CardTitle>
					<h2>{copy.repos.storageChecks}</h2>
				</CardTitle>
				<CardDescription>
					{checkedAt ? (
						<>
							{copy.repos.lastCheckedAt}{" "}
							<RelativeTime value={new Date(checkedAt)} />
						</>
					) : (
						copy.repos.neverChecked
					)}
				</CardDescription>
			</CardHeader>
			<CardContent className="flex flex-col gap-4">
				{error && (
					<Alert variant="destructive">
						<AlertDescription>{error}</AlertDescription>
					</Alert>
				)}
				<CapabilityChecklist rows={rows} />
			</CardContent>
			<CardFooter>
				<Button
					type="button"
					variant="secondary"
					disabled={recheck.isPending}
					onClick={() => recheck.mutate({ id: repositoryId })}
				>
					{recheck.isPending && <Spinner />}
					{recheck.isPending ? copy.check.running : copy.repos.checkAgain}
				</Button>
			</CardFooter>
		</Card>
	);
}
