import { useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import { CircleAlert, FlaskConical, Play, TriangleAlert } from "lucide-react";
import { type ReactNode, useEffect, useId, useRef } from "react";
import { toast } from "sonner";
import {
	getGetStatusQueryKey,
	getListJobsQueryKey,
	useAbort,
	usePutSettings,
	useStartDryRun,
	useStartRun,
} from "@/api/gen/endpoints";
import type { GcState, GcStatus, PreflightProblem } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import {
	CycleLog,
	ReclaimReport,
} from "@/components/repos/reclaim/reclaim-report";
import { ReclaimSettings } from "@/components/repos/reclaim/reclaim-settings";
import {
	dryRunActive,
	type GcFailure,
	gcFailure,
	STATUS_ID,
	settingsInput,
	useGcStatus,
} from "@/components/repos/reclaim/use-gc-status";
import { WriterMarkersAlert } from "@/components/repos/reclaim/writer-markers";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import {
	Field,
	FieldContent,
	FieldDescription,
	FieldLabel,
} from "@/components/ui/field";
import { Progress } from "@/components/ui/progress";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";
import { formatBytes, formatDateTime } from "@/lib/format";

export const RECLAIM_ANCHOR = "free-up-space";
export const BUSY = "aria-disabled:opacity-50";

const text = copy.gc;

function percent(done: number, total: number) {
	return total > 0
		? Math.min(100, Math.max(0, Math.round((done / total) * 100)))
		: undefined;
}

type Phase = Exclude<GcState, "waiting" | "done" | "failed" | "aborted">;

function Counter({ phase, status }: { phase: Phase; status: GcStatus }) {
	const progress = status.cycle?.progress;
	if (!progress || progress.total <= 0 || phase === "verifying") return null;
	const t = text.status;
	return (
		<p className="text-sm tabular-nums">
			{phase === "marking"
				? t.snapshots(progress.done, progress.total)
				: phase === "repacking"
					? t.repacked(
							progress.done,
							progress.total,
							formatBytes(progress.bytes),
						)
					: t.swept(progress.done, progress.total)}
		</p>
	);
}

export function ReclaimStatus({ status }: { status: GcStatus }) {
	const t = text.status;
	const cycle = status.cycle;
	let title: string;
	let line: ReactNode;
	let aside: ReactNode;
	let bar: ReactNode;
	let spinner = false;

	if (!cycle) {
		title = status.enabled ? t.idle : t.off;
		const next =
			status.settings.cron.trim() !== ""
				? status.schedule?.next_runs[0]
				: undefined;
		const [before, after] = t.nextRun;
		line =
			!status.enabled && !status.last_failure?.after_delete
				? t.offLine
				: next === undefined
					? t.noSchedule
					: null;
		const [abortedBefore, abortedAfter] = t.aborted;
		const abortedAt = status.last_aborted_at;
		const aborted =
			abortedAt != null &&
			abortedAt >
				Math.max(
					status.last_cleanup?.finished_at ?? 0,
					status.last_dry_run?.finished_at ?? 0,
				);
		aside = (
			<>
				{status.enabled && next !== undefined && (
					<p className="text-sm">
						{before}
						<RelativeTime value={new Date(next)} />
						{after}
					</p>
				)}
				{aborted && (
					<p className="text-sm text-muted-foreground">
						{abortedBefore}
						<RelativeTime value={new Date(abortedAt)} />
						{abortedAfter}
					</p>
				)}
			</>
		);
	} else if (cycle.dry_run) {
		title = t.dryRunTitle;
		spinner = true;
		line = t.phase[cycle.state as Phase];
		bar = (
			<Progress
				aria-label={t.progressLabel}
				value={percent(cycle.progress?.done ?? 0, cycle.progress?.total ?? 0)}
			/>
		);
		aside = <Counter phase={cycle.state as Phase} status={status} />;
	} else if (cycle.state === "waiting") {
		const [open, close] = t.parens;
		if (cycle.blocked_by_writers.length > 0) {
			title = t.phase.postponed;
			line = t.postponedLine(cycle.marked_packs);
		} else {
			title = t.phase.waiting;
			const end = cycle.delete_after;
			if (end != null) {
				const start = cycle.started_at ?? end;
				line = (
					<>
						{t.willDelete(
							cycle.marked_packs,
							formatBytes(cycle.marked_bytes),
							formatDateTime(end),
						)}{" "}
						{open}
						<RelativeTime value={new Date(end)} />
						{close}
					</>
				);
				bar = (
					<Progress
						aria-label={t.pauseLabel}
						value={percent(Date.now() - start, end - start) ?? 0}
					/>
				);
				aside = <p className="text-sm text-muted-foreground">{t.pauseNote}</p>;
			}
		}
	} else {
		const phase = cycle.state as Phase;
		title = t.phase[phase];
		line =
			phase === "verifying"
				? cycle.waiting_for_restores
					? t.restoresLine
					: t.verifying
				: null;
		bar = (
			<Progress
				aria-label={t.progressLabel}
				value={
					phase === "verifying"
						? undefined
						: percent(cycle.progress?.done ?? 0, cycle.progress?.total ?? 0)
				}
			/>
		);
		aside = (
			<>
				<Counter phase={phase} status={status} />
				{phase === "sweeping" && (
					<p className="text-sm text-muted-foreground">{t.sweepNote}</p>
				)}
			</>
		);
	}

	return (
		<div
			id={STATUS_ID}
			tabIndex={-1}
			className="flex flex-col gap-2 rounded-lg bg-muted p-4 outline-none"
		>
			<div
				role="status"
				aria-live="polite"
				className="flex flex-col gap-1 wrap-anywhere"
			>
				<p className="flex items-center gap-2 font-semibold">
					{spinner && <Spinner aria-hidden />}
					{title}
				</p>
				{line && <p className="text-sm text-muted-foreground">{line}</p>}
			</div>
			{bar}
			{aside}
			{cycle && !cycle.dry_run && status.active_dry_run && (
				<p className="text-sm text-muted-foreground">{t.alsoDryRun}</p>
			)}
		</div>
	);
}

function Actions({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: GcStatus;
}) {
	const client = useQueryClient();
	const hint = useId();
	const refresh = () =>
		Promise.all([
			client.invalidateQueries({
				queryKey: getGetStatusQueryKey(repositoryId),
			}),
			client.invalidateQueries({ queryKey: getListJobsQueryKey() }),
		]);
	const run = useStartRun({
		mutation: { onSuccess: refresh, onError: refresh },
	});
	const dry = useStartDryRun({
		mutation: { onSuccess: refresh, onError: refresh },
	});
	const abort = useAbort({
		mutation: {
			onSuccess: (next) => {
				client.setQueryData(getGetStatusQueryKey(repositoryId), next);
				refresh();
			},
			onError: (failure) => {
				refresh();
				toast.error(copy.errors.submit(text.abortFailed, failure.message));
			},
		},
	});
	const error = run.error ?? dry.error;
	const runBusy =
		run.isPending || (status.cycle != null && !status.cycle.dry_run);
	const dryBusy = dry.isPending || dryRunActive(status);
	const resumable = status.last_failure?.after_delete === true;
	const blocked = !resumable && status.preflight.problems.length > 0;
	const runOff = !status.enabled && !resumable;

	const cycleId = status.cycle?.id;
	const dryId = status.active_dry_run?.id;
	useEffect(() => {
		run.reset();
		dry.reset();
	}, [cycleId, dryId]);

	function start(which: typeof run | typeof dry, busy: boolean) {
		if (busy) return;
		run.reset();
		dry.reset();
		which.mutate({ id: repositoryId });
	}

	return (
		<div className="flex flex-col gap-2">
			{error && (
				<Alert variant="destructive">
					<CircleAlert aria-hidden />
					<AlertTitle>{text.startFailedTitle}</AlertTitle>
					<AlertDescription>
						<TextWithPaths
							text={copy.errors.submit(text.startFailed, error.message)}
						/>
					</AlertDescription>
				</Alert>
			)}
			<div className="flex flex-wrap items-center gap-2">
				<Button
					type="button"
					variant="outline"
					disabled={runOff || blocked}
					aria-disabled={runBusy || undefined}
					aria-describedby={runOff ? hint : undefined}
					className={BUSY}
					onClick={() => start(run, runBusy)}
				>
					{run.isPending ? <Spinner /> : <Play aria-hidden />}
					{text.run}
				</Button>
				<Button
					type="button"
					variant="outline"
					disabled={!status.preflight.dry_run_allowed}
					aria-disabled={dryBusy || undefined}
					className={BUSY}
					onClick={() => start(dry, dryBusy)}
				>
					{dry.isPending ? <Spinner /> : <FlaskConical aria-hidden />}
					{text.dryRun}
				</Button>
				{status.cycle && status.cycle.state !== "sweeping" && (
					<Button
						type="button"
						variant="outline"
						className={`ml-auto ${BUSY}`}
						aria-disabled={abort.isPending || undefined}
						onClick={() => {
							if (!abort.isPending) abort.mutate({ id: repositoryId });
						}}
					>
						{abort.isPending && <Spinner />}
						{text.abort}
					</Button>
				)}
			</div>
			{runOff && (
				<p id={hint} className="text-sm text-muted-foreground">
					{text.runOff}
				</p>
			)}
		</div>
	);
}

function ToggleRow({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: GcStatus;
}) {
	const client = useQueryClient();
	const id = useId();
	const key = getGetStatusQueryKey(repositoryId);
	const put = usePutSettings({
		mutation: {
			onSuccess: (next) => client.setQueryData(key, next),
			onError: (error) => {
				void client.invalidateQueries({ queryKey: key });
				if (error.status === 409) return;
				toast.error(
					copy.errors.submit(
						status.enabled ? text.turnOff : text.turnOn,
						error.message,
					),
				);
			},
		},
	});
	return (
		<Field orientation="horizontal">
			<FieldContent>
				<FieldLabel htmlFor={id}>{text.switchLabel}</FieldLabel>
				<FieldDescription id={`${id}-hint`}>{text.switchHint}</FieldDescription>
			</FieldContent>
			<Switch
				id={id}
				checked={status.enabled}
				aria-busy={put.isPending || undefined}
				aria-describedby={`${id}-hint`}
				onCheckedChange={(enabled) => {
					if (put.isPending) return;
					put.mutate({
						id: repositoryId,
						data: settingsInput(status, enabled),
					});
				}}
			/>
		</Field>
	);
}

const MAX_HOSTS = 5;

function Problem({
	problem,
	repositoryId,
}: {
	problem: PreflightProblem;
	repositoryId: string;
}) {
	const t = text.preflight;
	const hosts = problem.hosts ?? [];
	if (problem.kind === "old_agents" && hosts.length > 0) {
		const shown = hosts.slice(0, MAX_HOSTS);
		return (
			<>
				{t.oldAgents(hosts.length)}
				{shown.map((host, i) => (
					<span key={host.id}>
						{i > 0 && ", "}
						<Link
							to="/hosts/$id"
							params={{ id: host.id }}
							className="text-primary-strong underline"
						>
							{host.hostname}
						</Link>
					</span>
				))}
				{hosts.length > shown.length &&
					` ${t.more(hosts.length - shown.length)}`}
				{t.oldAgentsEnd}
			</>
		);
	}
	if (problem.kind === "server_key") return t.serverKey;
	if (problem.kind === "agent_key") {
		return (
			<>
				{t.agentKey}{" "}
				<Link
					to="/settings/repositories/$id"
					params={{ id: repositoryId }}
					hash="agent-key"
					className="text-primary-strong underline"
				>
					{t.openAgentKey}
				</Link>
			</>
		);
	}
	return <TextWithPaths text={problem.message} />;
}

export function ReclaimPreflight({
	repositoryId,
	problems,
}: {
	repositoryId: string;
	problems: PreflightProblem[];
}) {
	return (
		<Alert className="text-warning">
			<TriangleAlert aria-hidden />
			<AlertTitle>{text.preflight.title}</AlertTitle>
			<AlertDescription className="text-foreground">
				<ul className="flex flex-col gap-2">
					{problems.map((problem) => (
						<li
							key={`${problem.kind}:${problem.message}`}
							className="wrap-anywhere"
						>
							<Problem problem={problem} repositoryId={repositoryId} />
						</li>
					))}
				</ul>
			</AlertDescription>
		</Alert>
	);
}

function FailureAlert({ failure }: { failure: GcFailure }) {
	const t = text.failure;
	const after = failure.afterDelete;
	return (
		<Alert variant="destructive">
			<CircleAlert aria-hidden />
			<AlertTitle>{after ? t.titleAfterDelete : t.title}</AlertTitle>
			<AlertDescription className="flex flex-col gap-1 wrap-anywhere [&_p:not(:last-child)]:mb-0">
				<p>
					<TextWithPaths
						text={
							after ? t.bodyAfterDelete(failure.reason) : t.body(failure.reason)
						}
					/>
				</p>
				{!after && <p className="text-muted-foreground">{t.hint}</p>}
			</AlertDescription>
		</Alert>
	);
}

function ReclaimBody({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: GcStatus;
}) {
	const failure = gcFailure(status);
	return (
		<>
			<ToggleRow repositoryId={repositoryId} status={status} />
			{status.preflight.problems.length > 0 && (
				<ReclaimPreflight
					repositoryId={repositoryId}
					problems={status.preflight.problems}
				/>
			)}
			{status.cycle && status.cycle.blocked_by_writers.length > 0 && (
				<WriterMarkersAlert
					repositoryId={repositoryId}
					markers={status.cycle.blocked_by_writers}
				/>
			)}
			{failure && <FailureAlert failure={failure} />}
			<ReclaimStatus status={status} />
			<Actions repositoryId={repositoryId} status={status} />
			<Separator />
			<ReclaimSettings repositoryId={repositoryId} status={status} />
			<Separator />
			<ReclaimReport
				repositoryId={repositoryId}
				cleanup={status.last_cleanup}
				dryRun={status.last_dry_run}
				busy={dryRunActive(status)}
			/>
			{status.events_total > 0 && (
				<>
					<Separator />
					<CycleLog events={status.events} total={status.events_total} />
				</>
			)}
		</>
	);
}

export function ReclaimCard({ repositoryId }: { repositoryId: string }) {
	const query = useGcStatus(repositoryId);
	const hash = useRouterState({ select: (s) => s.location.hash });
	const heading = useRef<HTMLHeadingElement>(null);
	useEffect(() => {
		if (hash.replace(/^#/, "") !== RECLAIM_ANCHOR) return;
		heading.current?.closest("[data-slot=card]")?.scrollIntoView();
		heading.current?.focus({ preventScroll: true });
	}, [hash]);
	const status = query.data;

	return (
		<Card id={RECLAIM_ANCHOR} className="scroll-mt-20">
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<h2 ref={heading} tabIndex={-1} className="outline-none">
						{text.title}
					</h2>
					<Badge variant="outline">{text.beta}</Badge>
				</CardTitle>
				<CardDescription>{text.description}</CardDescription>
			</CardHeader>
			<CardContent className="@container flex flex-col gap-6">
				{status ? (
					<ReclaimBody repositoryId={repositoryId} status={status} />
				) : query.isError ? (
					<Alert variant="destructive">
						<CircleAlert aria-hidden />
						<AlertTitle>{text.loadFailedTitle}</AlertTitle>
						<AlertDescription className="flex flex-wrap items-center justify-between gap-4">
							{text.loadFailed}
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={() => query.refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				) : (
					<>
						<Skeleton className="h-24 w-full" />
						<Skeleton className="h-10 w-48" />
					</>
				)}
			</CardContent>
		</Card>
	);
}
