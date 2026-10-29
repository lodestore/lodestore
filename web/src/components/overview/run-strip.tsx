import type { RecentRun, RunOutcome } from "@/api/gen/model";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { cn } from "@/lib/utils";

const text = copy.hostBackup.runs;

const MARK: Record<RunOutcome, string> = {
	succeeded: "h-2 w-2 rounded-sm bg-success",
	failed: "h-4 w-2 rounded-sm bg-destructive",
	skipped: "h-2 w-2 rounded-sm border border-neutral",
};

export function RunStrip({ runs }: { runs: RecentRun[] }) {
	if (runs.length === 0) return <>{copy.common.dash}</>;
	return (
		<ul aria-label={text.label} className="flex h-4 items-end gap-1">
			{runs.map((run) => {
				const tick = text.tick(
					text.outcomes[run.outcome],
					formatDateTime(run.finished_at),
					run.policy,
				);
				return (
					<li
						key={`${run.finished_at}-${run.policy ?? ""}`}
						title={tick}
						className={MARK[run.outcome]}
					>
						<span className="sr-only">{tick}</span>
					</li>
				);
			})}
		</ul>
	);
}

export function RunLegend({ className }: { className?: string }) {
	return (
		<p
			className={cn(
				"flex flex-wrap items-center gap-4 text-sm text-muted-foreground",
				className,
			)}
		>
			{(Object.keys(MARK) as RunOutcome[]).map((outcome) => (
				<span key={outcome} className="flex items-center gap-2">
					<span aria-hidden className={cn("inline-block", MARK[outcome])} />
					{text.outcomes[outcome]}
				</span>
			))}
			<span>{text.newestRight}</span>
		</p>
	);
}
