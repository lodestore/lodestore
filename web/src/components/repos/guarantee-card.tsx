import { useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { type ReactNode, useState } from "react";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useRecheck,
} from "@/api/gen/endpoints";
import type { RepositoryDetail } from "@/api/gen/model";
import { LevelLine, LevelScale } from "@/components/guarantee-level";
import { levelName } from "@/components/guarantee-notes";
import { RelativeTime } from "@/components/relative-time";
import { isBelowAccepted } from "@/components/repos/guarantee-banner";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

function why(repository: RepositoryDetail): string {
	const [code] = repository.guarantee_notes;
	if (code === undefined)
		return (
			(copy.guarantee.whyBackend as Record<string, string>)[
				repository.backend
			] ?? copy.common.dash
		);
	return (
		(copy.guarantee.notes as Record<string, string>)[code] ??
		copy.guarantee.unknownNote
	);
}

export function GuaranteeCard({
	repository,
	actions,
}: {
	repository: RepositoryDetail;
	actions?: ReactNode;
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
						queryKey: getGetRepositoryQueryKey(repository.id),
					}),
					queryClient.invalidateQueries({
						queryKey: getListRepositoriesQueryKey(),
					}),
				]),
		},
	});
	const checkedAt = repository.guarantee_checked_at;

	return (
		<Card id="guarantee">
			<CardHeader>
				<CardTitle>
					<h2>{copy.guarantee.cardTitle}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-4 md:flex-row md:justify-between">
				<div className="flex min-w-0 flex-col gap-4 md:max-w-xl md:flex-1">
					<LevelScale level={repository.guarantee_level} />
					<dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm">
						<dt className="text-muted-foreground">{copy.guarantee.measured}</dt>
						<dd>
							<LevelLine
								level={repository.guarantee_level}
								warn={isBelowAccepted(
									repository.guarantee_level,
									repository.guarantee_accepted,
								)}
							/>
						</dd>
						<dt className="text-muted-foreground">{copy.guarantee.accepted}</dt>
						<dd>{levelName(repository.guarantee_accepted)}</dd>
						<dt className="text-muted-foreground">{copy.guarantee.checked}</dt>
						<dd>
							{checkedAt != null ? (
								<>
									<RelativeTime value={new Date(checkedAt)} /> ·{" "}
									{copy.guarantee.daily}
								</>
							) : (
								copy.guarantee.neverChecked
							)}
						</dd>
						<dt className="text-muted-foreground">{copy.guarantee.why}</dt>
						<dd>{why(repository)}</dd>
					</dl>
				</div>
				<div className="flex flex-col items-start gap-2 md:items-end">
					{actions}
					<Button
						type="button"
						variant="outline"
						disabled={recheck.isPending}
						onClick={() => recheck.mutate({ id: repository.id })}
					>
						{recheck.isPending ? <Spinner /> : <RefreshCw aria-hidden />}
						{copy.guarantee.checkNow}
					</Button>
					{error && (
						<p role="alert" className="text-sm text-destructive">
							{error}
						</p>
					)}
				</div>
			</CardContent>
		</Card>
	);
}
