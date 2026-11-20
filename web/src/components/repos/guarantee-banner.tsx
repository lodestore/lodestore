import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Info, TriangleAlert } from "lucide-react";
import { useState } from "react";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useAcceptGuarantee,
} from "@/api/gen/endpoints";
import type { RepositoryDetail } from "@/api/gen/model";
import {
	GuaranteeNotes,
	levelName,
	UNCHECKED_LEVEL,
	WEAK_LEVEL,
} from "@/components/guarantee-notes";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const STRENGTH: Record<string, number> = {
	[WEAK_LEVEL]: 0,
	overwrite_keeps_old_version: 1,
	create_only_enforced: 2,
};

export type GuaranteeBannerKind = "unchecked" | "downgrade" | "weak" | "notes";

export function guaranteeBannerKind(
	repository: Pick<
		RepositoryDetail,
		"guarantee_level" | "guarantee_accepted" | "guarantee_notes"
	>,
): GuaranteeBannerKind | undefined {
	const { guarantee_level: level, guarantee_accepted: accepted } = repository;
	if (level === UNCHECKED_LEVEL) return "unchecked";
	const lower =
		accepted === UNCHECKED_LEVEL
			? level === WEAK_LEVEL
			: (STRENGTH[level] ?? 0) < (STRENGTH[accepted] ?? 0);
	if (lower) return "downgrade";
	if (level === WEAK_LEVEL) return "weak";
	return repository.guarantee_notes.length > 0 ? "notes" : undefined;
}

export function GuaranteeBanner({
	repository,
}: {
	repository: RepositoryDetail;
}) {
	const queryClient = useQueryClient();
	const [error, setError] = useState<string>();
	const accept = useAcceptGuarantee({
		mutation: {
			onMutate: () => setError(undefined),
			onError: (e) =>
				setError(copy.errors.submit(copy.guarantee.acceptFailed, e.message)),
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
	const kind = guaranteeBannerKind(repository);
	if (!kind) return null;

	const title = {
		unchecked: copy.guarantee.uncheckedTitle,
		downgrade: copy.guarantee.downgradeTitle,
		weak: copy.guarantee.weakTitle,
		notes: copy.guarantee.notesTitle,
	}[kind];

	return (
		<Alert className={kind === "notes" ? undefined : "text-warning"}>
			{kind === "notes" ? <Info aria-hidden /> : <TriangleAlert aria-hidden />}
			<AlertTitle>{title}</AlertTitle>
			<AlertDescription className="flex flex-col gap-4">
				{kind === "unchecked" && <p>{copy.guarantee.uncheckedBody}</p>}
				{kind === "downgrade" && (
					<p>
						{copy.guarantee.downgradeBody(
							levelName(repository.guarantee_accepted),
							levelName(repository.guarantee_level),
						)}
					</p>
				)}
				<GuaranteeNotes codes={repository.guarantee_notes} />
				{kind === "unchecked" && (
					<Link
						to="/settings/repositories/$id"
						params={{ id: repository.id }}
						hash="storage-checks"
						className="w-fit text-primary-strong underline"
					>
						{copy.guarantee.seeChecks}
					</Link>
				)}
				{kind === "downgrade" && (
					<div>
						<Button
							type="button"
							variant="outline"
							disabled={accept.isPending}
							onClick={() => accept.mutate({ id: repository.id })}
						>
							{accept.isPending && <Spinner />}
							{accept.isPending
								? copy.guarantee.accepting
								: copy.guarantee.acceptNewLevel}
						</Button>
					</div>
				)}
				{error && <p role="alert">{error}</p>}
			</AlertDescription>
		</Alert>
	);
}
