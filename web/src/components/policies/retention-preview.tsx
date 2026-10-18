import { Link } from "@tanstack/react-router";
import { ChevronDown, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";
import { useRetentionPreview } from "@/api/gen/endpoints";
import type {
	Retention,
	RetentionPreview as RetentionPreviewData,
} from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { hostCount, snapshotCount } from "@/lib/plural";
import { cn } from "@/lib/utils";

const DEBOUNCE_MS = 400;
const HOSTS_SHOWN = 10;
const HEAVY_SHARE = 0.5;

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
}: {
	state: PreviewState;
	isNew: boolean;
	hasRules: boolean;
	changed: boolean;
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
				{forget > 0 && changed && (
					<p className="text-sm text-muted-foreground">{text.savingNote}</p>
				)}
			</div>
		</div>
	);
}
