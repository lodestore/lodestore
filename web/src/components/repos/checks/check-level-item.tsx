import { useQueryClient } from "@tanstack/react-query";
import { ChevronDown, Play, X } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import {
	getGetCheckStatusQueryKey,
	getListJobsQueryKey,
	useCancelCheck,
	useStartCheck,
} from "@/api/gen/endpoints";
import type {
	CheckLevelStatus,
	CheckRunDto,
	PackCoverage,
} from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { CheckReport } from "@/components/repos/checks/check-report";
import { levelFailed } from "@/components/repos/checks/use-check-status";
import { type Status, StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Item,
	ItemActions,
	ItemContent,
	ItemDescription,
	ItemTitle,
} from "@/components/ui/item";
import { Progress } from "@/components/ui/progress";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatBytes, formatCount } from "@/lib/format";

const text = copy.checks;

const BUSY = "aria-disabled:opacity-50";

function badgeStatus(level: CheckLevelStatus): Status {
	if (level.running)
		return level.running.state === "queued" ? "queued" : "running";
	if (levelFailed(level)) return "failed";
	if (level.last?.state === "aborted") return "cancelled";
	if (level.last) return "passed";
	return level.settings.enabled ? "never_run" : "neutral";
}

function percent(done: number, total: number) {
	return total > 0
		? Math.min(100, Math.max(0, Math.round((done / total) * 100)))
		: undefined;
}

function RunningStatus({ run }: { run: CheckRunDto }) {
	const progress = run.progress;
	const total = progress?.total ?? 0;
	const ready = progress != null && total > 0;
	const counter =
		run.level === "packs"
			? ready
				? text.counter.packs(
						formatCount(progress.done),
						formatCount(total),
						formatBytes(progress.bytes),
					)
				: null
			: run.level === "restore"
				? ready
					? text.counter.files(formatCount(progress.done), formatCount(total))
					: null
				: progress?.phase === "snapshots"
					? text.counter.snapshots
					: text.counter.listing;
	return (
		<div className="flex basis-full flex-col gap-2 rounded-lg bg-muted p-4">
			<div role="status" aria-live="polite">
				<p className="font-semibold">{text.levels[run.level].running}</p>
			</div>
			<Progress
				aria-label={text.progressLabel}
				value={percent(progress?.done ?? 0, total)}
			/>
			{counter && <p className="text-sm tabular-nums">{counter}</p>}
		</div>
	);
}

function LevelLine({ level }: { level: CheckLevelStatus }) {
	const [before, after] = text.status.nextRun;
	const finished = level.last?.finished_at;
	const next = level.settings.enabled
		? level.schedule?.next_runs[0]
		: undefined;
	const reason =
		levelFailed(level) &&
		(level.last?.state === "failed" || level.last?.outcome === "stopped") &&
		level.last.error
			? text.status.stopped(level.last.error.split("\n")[0])
			: null;
	return (
		<div className="flex flex-col text-sm text-muted-foreground">
			{reason && (
				<span className="line-clamp-3 wrap-anywhere">
					<TextWithPaths text={reason} />
				</span>
			)}
			{finished != null && <RelativeTime value={new Date(finished)} />}
			{!level.settings.enabled ? (
				<span>{text.status.off}</span>
			) : next !== undefined ? (
				<span>
					{before}
					<RelativeTime value={new Date(next)} />
					{after}
				</span>
			) : (
				<span>{text.status.notScheduled}</span>
			)}
		</div>
	);
}

export function CheckLevelItem({
	repositoryId,
	level,
	coverage,
	blocked,
	hintId,
	onStartError,
}: {
	repositoryId: string;
	level: CheckLevelStatus;
	coverage?: PackCoverage | null;
	blocked: boolean;
	hintId: string;
	onStartError: (message: string | null) => void;
}) {
	const client = useQueryClient();
	const refresh = () =>
		Promise.all([
			client.invalidateQueries({
				queryKey: getGetCheckStatusQueryKey(repositoryId),
			}),
			client.invalidateQueries({ queryKey: getListJobsQueryKey() }),
		]);
	const start = useStartCheck({
		mutation: {
			onSuccess: () => {
				onStartError(null);
				return refresh();
			},
			onError: (failure) => {
				onStartError(failure.message);
				return refresh();
			},
		},
	});
	const cancel = useCancelCheck({
		mutation: {
			onSuccess: refresh,
			onError: (failure) => {
				void refresh();
				toast.error(copy.errors.submit(text.cancelFailed, failure.message));
			},
		},
	});

	const running = level.running;
	const last = level.last;
	const failed = levelFailed(level);
	const [picked, setPicked] = useState<{ run?: string; open: boolean }>({
		open: false,
	});
	const open = picked.run === last?.id ? picked.open : failed;
	const status = badgeStatus(level);
	const busy = start.isPending || cancel.isPending;

	function click() {
		if (busy) return;
		if (running) {
			cancel.mutate({ id: repositoryId, runId: running.id });
			return;
		}
		if (blocked) return;
		onStartError(null);
		start.mutate({ id: repositoryId, level: level.level });
	}

	return (
		<Item variant="outline" className="items-start">
			<ItemContent>
				<ItemTitle>{text.levels[level.level].name}</ItemTitle>
				<ItemDescription>{text.levels[level.level].line}</ItemDescription>
			</ItemContent>
			<ItemContent className="flex-none gap-1">
				<StatusBadge status={status}>
					{status === "neutral" ? text.status.off : undefined}
				</StatusBadge>
				<LevelLine level={level} />
			</ItemContent>
			<ItemActions>
				<Button
					type="button"
					variant="outline"
					className={BUSY}
					aria-disabled={busy || (!running && blocked) || undefined}
					aria-describedby={!running && blocked ? hintId : undefined}
					onClick={click}
				>
					{busy ? (
						<Spinner />
					) : running ? (
						<X aria-hidden />
					) : (
						<Play aria-hidden />
					)}
					{running ? text.cancel : text.run}
				</Button>
			</ItemActions>
			{running && <RunningStatus run={running} />}
			{last && (
				<Collapsible
					open={open}
					onOpenChange={(next) => setPicked({ run: last.id, open: next })}
					className="flex basis-full flex-col gap-2"
				>
					<CollapsibleTrigger asChild>
						<Button
							type="button"
							variant="outline"
							size="sm"
							className="group self-start"
						>
							<ChevronDown
								aria-hidden
								className="transition-transform group-data-[state=closed]:-rotate-90"
							/>
							{open ? text.hideReport : text.showReport}
						</Button>
					</CollapsibleTrigger>
					<CollapsibleContent>
						<CheckReport
							repositoryId={repositoryId}
							run={last}
							coverage={last.level === "packs" ? coverage : null}
						/>
					</CollapsibleContent>
				</Collapsible>
			)}
		</Item>
	);
}
