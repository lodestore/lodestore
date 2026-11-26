import { Link } from "@tanstack/react-router";
import { Clock } from "lucide-react";
import type { ReactNode } from "react";
import type { RepositorySummary } from "@/api/gen/model";
import { LevelLine } from "@/components/guarantee-level";
import { RelativeTime } from "@/components/relative-time";
import {
	guaranteeAge,
	isBelowAccepted,
} from "@/components/repos/guarantee-banner";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";

const DAY_MS = 24 * 60 * 60 * 1000;

function Warning({ children }: { children: ReactNode }) {
	return (
		<span className="inline-flex items-center gap-1 text-warning">
			<Clock aria-hidden className="size-4 shrink-0" />
			{children}
		</span>
	);
}

export function GuaranteeCell({
	repository,
}: {
	repository: Pick<
		RepositorySummary,
		"id" | "guarantee_level" | "guarantee_accepted" | "guarantee_checked_at"
	>;
}) {
	const now = Date.now();
	const checkedAt = repository.guarantee_checked_at;
	const age = guaranteeAge(repository, now);
	const when = checkedAt != null && (
		<RelativeTime value={new Date(checkedAt)} />
	);

	return (
		<div className="flex flex-col gap-1">
			<LevelLine
				level={repository.guarantee_level}
				warn={isBelowAccepted(
					repository.guarantee_level,
					repository.guarantee_accepted,
				)}
			/>
			{age !== "none" && (
				<span className="text-sm text-muted-foreground">
					{age === "lower" && (
						<span className="inline-flex flex-wrap items-center gap-x-1">
							<span className="text-warning">
								{copy.guarantee.lowerThanAccepted}
							</span>
							<span aria-hidden>·</span>
							<Link
								to="/settings/repositories/$id"
								params={{ id: repository.id }}
								hash="guarantee"
								className="text-primary-strong underline"
							>
								{copy.guarantee.seeAlert}
							</Link>
						</span>
					)}
					{age === "never" && <Warning>{copy.guarantee.neverChecked}</Warning>}
					{age === "stale" && checkedAt != null && (
						<Tooltip>
							<TooltipTrigger asChild>
								<span tabIndex={0}>
									<Warning>
										{copy.guarantee.notCheckedFor(
											Math.floor((now - checkedAt) / DAY_MS),
										)}
									</Warning>
								</span>
							</TooltipTrigger>
							<TooltipContent>{formatDateTime(checkedAt)}</TooltipContent>
						</Tooltip>
					)}
					{age === "accepted" && (
						<>
							{copy.guarantee.acceptedChecked} {when}
						</>
					)}
					{age === "checked" && (
						<>
							{copy.guarantee.checkedPrefix} {when}
						</>
					)}
				</span>
			)}
		</div>
	);
}
