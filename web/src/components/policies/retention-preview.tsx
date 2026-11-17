import { Link } from "@tanstack/react-router";
import { ChevronDown, ChevronRight, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useRetentionPreview } from "@/api/gen/endpoints";
import type {
	Retention,
	RetentionPreview as RetentionPreviewData,
} from "@/api/gen/model";
import { Truncated } from "@/components/hosts/hosts-table";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { hostCount, snapshotCount, sourceCount } from "@/lib/plural";
import {
	type SourceType,
	sourceTypeIcon,
	sourceTypeLabel,
} from "@/lib/source-type";
import { cn } from "@/lib/utils";

const DEBOUNCE_MS = 400;
const HOSTS_SHOWN = 10;
const HEAVY_SHARE = 0.5;
const SOURCES_PER_PAGE = 25;
const ALL_HOSTS = "__all__";

export type PreviewState = {
	active: boolean;
	data?: RetentionPreviewData;
	fresh: boolean;
	loading: boolean;
	failed: boolean;
	retry: () => void;
};

export function useRetentionPreviewState({
	name,
	retention,
	timezone,
	enabled,
}: {
	name?: string;
	retention?: Retention;
	timezone: string;
	enabled: boolean;
}): PreviewState {
	const preview = useRetentionPreview();
	const active = enabled && name !== undefined && retention !== undefined;
	const key = JSON.stringify([retention, timezone]);
	const [result, setResult] = useState<{
		key: string;
		data: RetentionPreviewData;
	}>();
	const [failure, setFailure] = useState<{ key: string }>();
	const [attempt, setAttempt] = useState(0);

	useEffect(() => {
		if (!active || name === undefined || retention === undefined) return;
		const timer = setTimeout(() => {
			preview.mutate(
				{ name, data: { retention, timezone } },
				{
					onSuccess: (data) => {
						setFailure(undefined);
						setResult({ key, data });
					},
					onError: () => setFailure({ key }),
				},
			);
		}, DEBOUNCE_MS);
		return () => clearTimeout(timer);
	}, [active, key, name, attempt]);

	const fresh = active && result?.key === key;
	const failed = active && !fresh && failure?.key === key;
	return {
		active,
		data: result?.data,
		fresh,
		loading: active && !fresh && !failed,
		failed,
		retry: () => {
			setFailure(undefined);
			setAttempt((n) => n + 1);
		},
	};
}

export function forgetTotals(data: RetentionPreviewData) {
	return {
		forget: data.forget,
		hosts: data.hosts.filter((host) => host.forget > 0).length,
	};
}

function ForgetList({ data }: { data: RetentionPreviewData }) {
	const text = copy.policies.retention.preview;
	const [all, setAll] = useState(false);
	const [open, setOpen] = useState(false);
	const hosts = data.hosts.filter((host) => host.forget > 0);
	const shown = all ? hosts : hosts.slice(0, HOSTS_SHOWN);
	return (
		<Collapsible
			open={open}
			onOpenChange={setOpen}
			className="flex flex-col gap-2"
		>
			<CollapsibleTrigger asChild>
				<Button
					type="button"
					variant="outline"
					size="sm"
					className="group/forget self-start"
				>
					<ChevronDown
						aria-hidden
						className="transition-transform group-data-[state=closed]/forget:-rotate-90"
					/>
					{open ? text.hide : text.show}
				</Button>
			</CollapsibleTrigger>
			<CollapsibleContent className="flex flex-col gap-2">
				<ul className="flex flex-col">
					{shown.map((host) => (
						<li key={host.host_id} className="border-t first:border-t-0">
							<div className="flex min-h-12 items-center justify-between gap-4">
								<Link
									to="/hosts/$id"
									params={{ id: host.host_id }}
									className="min-w-0 font-semibold text-primary-strong wrap-anywhere underline-offset-4 hover:underline"
								>
									{host.hostname}
								</Link>
								<span className="shrink-0 text-sm text-muted-foreground">
									{text.forgetOf(host.forget, host.total)}
								</span>
							</div>
							<ul className="flex flex-col gap-1 pb-4">
								{host.snapshots.map((snapshot) => (
									<li
										key={snapshot.id}
										className="flex flex-wrap items-baseline gap-x-4 text-sm"
									>
										<time dateTime={new Date(snapshot.time).toISOString()}>
											{formatDateTime(snapshot.time)}
										</time>
										<span className="font-mono">{snapshot.id.slice(0, 8)}</span>
										<span className="text-muted-foreground">
											{text.notKept}
										</span>
									</li>
								))}
							</ul>
						</li>
					))}
				</ul>
				{!all && hosts.length > HOSTS_SHOWN && (
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="self-start"
						onClick={() => setAll(true)}
					>
						{text.showAll(hosts.length)}
					</Button>
				)}
				{data.truncated > 0 && (
					<p className="text-sm text-muted-foreground">
						{text.more(data.truncated)}
					</p>
				)}
			</CollapsibleContent>
		</Collapsible>
	);
}

export function RetentionPreview({
	state,
	isNew,
	hasRules,
	changed,
	onOpen,
}: {
	state: PreviewState;
	isNew: boolean;
	hasRules: boolean;
	changed: boolean;
	onOpen?: () => void;
}) {
	const text = copy.policies.retention.preview;
	if (!hasRules) return null;
	if (isNew) {
		return <p className="text-sm text-muted-foreground">{text.newPolicy}</p>;
	}
	if (!state.active) return null;
	if (state.failed) {
		return (
			<Alert variant="destructive" role="alert">
				<AlertDescription className="flex flex-wrap items-center justify-between gap-2">
					{text.error}
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={state.retry}
					>
						{text.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}
	const { data } = state;
	if (!data) {
		return (
			<div
				aria-live="polite"
				aria-busy
				className="flex items-center gap-2 rounded-lg bg-muted p-4 text-sm text-muted-foreground"
			>
				<Spinner />
				{text.working}
			</div>
		);
	}
	const { forget, hosts } = forgetTotals(data);
	const heavy = data.total > 0 && forget / data.total >= HEAVY_SHARE;
	const bySource = data.multi_instance && onOpen !== undefined;
	return (
		<div
			aria-live="polite"
			aria-busy={state.loading}
			className="flex flex-col gap-2 rounded-lg bg-muted p-4"
		>
			{state.loading && (
				<p className="flex items-center gap-2 text-sm text-muted-foreground">
					<Spinner />
					{text.working}
				</p>
			)}
			<div className={cn("flex flex-col gap-2", state.loading && "opacity-60")}>
				{heavy && (
					<Alert role="status" className="border-warning/60">
						<TriangleAlert aria-hidden className="text-warning" />
						<AlertDescription className="text-foreground">
							{text.heavy(Math.round((forget / data.total) * 100))}
						</AlertDescription>
					</Alert>
				)}
				{bySource ? (
					<div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
						<p className="text-sm font-semibold">
							{forget === 0
								? text.nothing
								: text.sourcesSummary(
										String(forget),
										snapshotCount(data.total),
										sourceCount(data.instances.length),
										hostCount(distinctHosts(data)),
									)}
						</p>
						{forget > 0 && (
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={onOpen}
							>
								{text.open}
							</Button>
						)}
					</div>
				) : (
					<>
						<p className="text-sm font-semibold">
							{forget === 0
								? text.nothing
								: text.summary(
										String(forget),
										snapshotCount(data.total),
										hostCount(hosts),
									)}
						</p>
						{data.floor > 0 && (
							<p className="text-sm text-muted-foreground">
								{text.floor(data.floor)}
							</p>
						)}
						{data.suspicious > 0 && (
							<p className="text-sm text-muted-foreground">
								{text.suspicious(data.suspicious)}
							</p>
						)}
						{forget > 0 && <ForgetList data={data} />}
					</>
				)}
				{forget > 0 && changed && (
					<p className="text-sm text-muted-foreground">{text.savingNote}</p>
				)}
			</div>
		</div>
	);
}

const distinctHosts = (data: RetentionPreviewData) =>
	new Set(data.instances.map((instance) => instance.host_id)).size;

function describeRules(rules: Retention | undefined) {
	const text = copy.policies.retention.preview.rules;
	if (!rules) return "";
	return [
		rules.keepLast != null && text.last(rules.keepLast),
		rules.keepWithin != null && text.within(rules.keepWithin),
		rules.keepDaily != null && text.daily(rules.keepDaily),
		rules.keepWeekly != null && text.weekly(rules.keepWeekly),
		rules.keepMonthly != null && text.monthly(rules.keepMonthly),
	]
		.filter(Boolean)
		.join(", ");
}

function InstanceRow({
	instance,
}: {
	instance: RetentionPreviewData["instances"][number];
}) {
	const text = copy.policies.retention.preview;
	const Icon =
		sourceTypeIcon[instance.source_type as SourceType] ?? sourceTypeIcon.files;
	const label = instance.source_label ?? sourceTypeLabel(instance.source_type);
	return (
		<Collapsible asChild>
			<TableBody>
				<TableRow>
					<TableCell className="max-w-0 min-w-40">
						<CollapsibleTrigger asChild>
							<button
								type="button"
								aria-label={text.expand(label)}
								className="group/row flex w-full min-w-0 items-center gap-2 rounded-sm text-left outline-none focus-visible:ring-2 focus-visible:ring-ring"
							>
								<ChevronRight
									aria-hidden
									className="size-4 shrink-0 text-muted-foreground transition-transform group-data-[state=open]/row:rotate-90"
								/>
								<Icon
									aria-hidden
									className="size-4 shrink-0 text-muted-foreground"
								/>
								<Truncated text={label} className="font-semibold" />
							</button>
						</CollapsibleTrigger>
					</TableCell>
					<TableCell className="hidden @min-[680px]:table-cell">
						<Link
							to="/hosts/$id"
							params={{ id: instance.host_id }}
							className="font-semibold text-primary-strong wrap-anywhere underline-offset-4 hover:underline"
						>
							{instance.hostname}
						</Link>
					</TableCell>
					<TableCell className="text-right tabular-nums">
						{instance.forget}
					</TableCell>
					<TableCell className="text-right tabular-nums">
						{instance.keep}
					</TableCell>
				</TableRow>
				<CollapsibleContent asChild>
					<TableRow>
						<TableCell colSpan={4} className="pt-0 pb-4 pl-10">
							<ul className="flex flex-col gap-1">
								{instance.snapshots.map((snapshot) => (
									<li
										key={snapshot.id}
										className="flex flex-wrap items-baseline gap-x-4 text-sm"
									>
										<time dateTime={new Date(snapshot.time).toISOString()}>
											{formatDateTime(snapshot.time)}
										</time>
										<span className="font-mono">{snapshot.id.slice(0, 8)}</span>
										<span className="text-muted-foreground">
											{text.notKept}
										</span>
									</li>
								))}
							</ul>
						</TableCell>
					</TableRow>
				</CollapsibleContent>
			</TableBody>
		</Collapsible>
	);
}

export function InstancePreview({
	state,
	rules,
	changed,
}: {
	state: PreviewState;
	rules?: Retention;
	changed: boolean;
}) {
	const text = copy.policies.retention.preview;
	const columns = text.columns;
	const [host, setHost] = useState(ALL_HOSTS);
	const [page, setPage] = useState(1);
	const { data } = state;
	if (state.failed || !data) {
		return state.failed ? (
			<Alert variant="destructive" role="alert">
				<AlertDescription className="flex flex-wrap items-center justify-between gap-2">
					{text.error}
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={state.retry}
					>
						{text.retry}
					</Button>
				</AlertDescription>
			</Alert>
		) : null;
	}

	const hosts = [
		...new Map(
			data.instances.map((instance) => [instance.host_id, instance.hostname]),
		),
	];
	const filtered =
		host === ALL_HOSTS
			? data.instances
			: data.instances.filter((instance) => instance.host_id === host);
	const lastPage = Math.max(1, Math.ceil(filtered.length / SOURCES_PER_PAGE));
	const current = Math.min(page, lastPage);
	const first = (current - 1) * SOURCES_PER_PAGE;
	const shown = filtered.slice(first, first + SOURCES_PER_PAGE);
	const hostLabel =
		host === ALL_HOSTS
			? text.allHosts
			: (hosts.find(([id]) => id === host)?.[1] ?? text.allHosts);
	const filterSummary = `${text.hostFilter}: ${hostLabel}`;

	return (
		<div
			aria-live="polite"
			aria-busy={state.loading}
			className="@container flex flex-col gap-4"
		>
			{state.loading && (
				<p className="flex items-center gap-2 text-sm text-muted-foreground">
					<Spinner />
					{text.working}
				</p>
			)}
			<div className={cn("flex flex-col gap-4", state.loading && "opacity-60")}>
				<div className="flex flex-col gap-1">
					<p className="text-base font-semibold">
						{data.forget === 0
							? text.nothing
							: text.sourcesSummary(
									String(data.forget),
									snapshotCount(data.total),
									sourceCount(data.instances.length),
									hostCount(distinctHosts(data)),
								)}
					</p>
					{changed && (
						<p className="text-sm text-muted-foreground">
							{text.unsavedRules(describeRules(rules))}
						</p>
					)}
					{data.floor > 0 && (
						<p className="text-sm text-muted-foreground">
							{text.floor(data.floor, "source")}
						</p>
					)}
					{data.suspicious > 0 && (
						<p className="text-sm text-muted-foreground">
							{text.suspicious(data.suspicious)}
						</p>
					)}
				</div>
				{data.forget > 0 && data.instances.length > 0 && (
					<>
						<div className="flex flex-col gap-2 @min-[680px]:flex-row @min-[680px]:items-center @min-[680px]:justify-between @min-[680px]:gap-4">
							<Select
								value={host}
								onValueChange={(value) => {
									setHost(value);
									setPage(1);
								}}
							>
								<SelectTrigger
									aria-label={filterSummary}
									className="w-full data-[size=default]:h-10 @min-[680px]:w-60"
								>
									<SelectValue>{filterSummary}</SelectValue>
								</SelectTrigger>
								<SelectContent>
									<SelectItem value={ALL_HOSTS}>{text.allHosts}</SelectItem>
									{hosts.map(([id, hostname]) => (
										<SelectItem key={id} value={id}>
											{hostname}
										</SelectItem>
									))}
								</SelectContent>
							</Select>
							<p className="text-sm text-muted-foreground">{text.sorted}</p>
						</div>
						<div className="overflow-hidden rounded-lg border bg-card">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>{columns.source}</TableHead>
										<TableHead className="hidden @min-[680px]:table-cell">
											{columns.host}
										</TableHead>
										<TableHead className="text-right">
											{columns.forget}
										</TableHead>
										<TableHead className="text-right">{columns.keep}</TableHead>
									</TableRow>
								</TableHeader>
								{shown.map((instance, i) => (
									<InstanceRow
										key={`${instance.host_id}-${instance.source_type}-${instance.source_label}-${first + i}`}
										instance={instance}
									/>
								))}
							</Table>
						</div>
						<div className="flex flex-wrap items-center justify-between gap-2">
							<p className="text-sm text-muted-foreground">
								{text.showingSources(
									first + 1,
									first + shown.length,
									sourceCount(filtered.length),
								)}
							</p>
							<div className="flex gap-2">
								<Button
									type="button"
									variant="outline"
									size="sm"
									disabled={current <= 1}
									onClick={() => setPage(current - 1)}
								>
									{copy.jobs.previous}
								</Button>
								<Button
									type="button"
									variant="outline"
									size="sm"
									disabled={current >= lastPage}
									onClick={() => setPage(current + 1)}
								>
									{copy.jobs.next}
								</Button>
							</div>
						</div>
						{data.instances_truncated > 0 && (
							<p className="text-sm text-muted-foreground">
								{text.moreSnapshots(data.instances_truncated)}
							</p>
						)}
					</>
				)}
			</div>
		</div>
	);
}
