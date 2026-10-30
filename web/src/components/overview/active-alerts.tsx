import { Link } from "@tanstack/react-router";
import { ChevronDown, CircleX, TriangleAlert } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { useListAlerts } from "@/api/gen/endpoints";
import type { AlertDto } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { formatRelative, RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	Item,
	ItemActions,
	ItemContent,
	ItemDescription,
	ItemGroup,
	ItemMedia,
	ItemTitle,
} from "@/components/ui/item";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { cn } from "@/lib/utils";

const REFRESH_MS = 10_000;
const SHOWN = 5;
const MAX = 50;
const DAY_MS = 86_400_000;
const SLOT = "\u0000";
const text = copy.overview.alerts;
const linkClass =
	"rounded-sm text-primary-strong underline-offset-4 hover:underline";

export function order(a: AlertDto, b: AlertDto) {
	const rank = (alert: AlertDto) =>
		(alert.silenced ? 2 : 0) + (alert.severity === "critical" ? 0 : 1);
	return rank(a) - rank(b) || b.started_at - a.started_at;
}

function relative(ms?: number | null) {
	return ms == null ? undefined : formatRelative(new Date(ms), new Date());
}

function titleOf(alert: AlertDto, now: number) {
	const hostname = alert.host?.hostname ?? text.unknownHost;
	const hostLink = alert.host ? (
		<Link to="/hosts/$id" params={{ id: alert.host.id }} className={linkClass}>
			{hostname}
		</Link>
	) : (
		hostname
	);
	const repository = alert.repository?.name ?? text.unknownRepository;
	const repositoryLink = alert.repository ? (
		<Link
			to="/settings/repositories/$id"
			params={{ id: alert.repository.id }}
			hash="checks"
			className={linkClass}
		>
			{repository}
		</Link>
	) : (
		repository
	);
	const titles = text.titles;

	switch (alert.type) {
		case "backup_failed":
			return { build: titles.backupFailed, link: hostLink };
		case "no_recent_backup":
			return { build: titles.noRecent, link: hostLink };
		case "agent_offline":
			return { build: titles.offline, link: hostLink };
		case "agent_update_failed":
			return { build: titles.agentUpdateFailed, link: hostLink };
		case "check_failed": {
			const level = alert.check_level ?? "";
			const name =
				text.levels[level] ?? level.charAt(0).toUpperCase() + level.slice(1);
			return {
				build: (slot: string) => titles.checkFailed(name, slot),
				link: repositoryLink,
			};
		}
		case "certificate_expiring": {
			const notAfter = alert.facts.not_after;
			if (notAfter == null) return { build: () => alert.title, link: null };
			const grpc = alert.cert === "grpc";
			const name = grpc
				? text.certificates.grpc
				: text.certificates.agent(SLOT);
			const build = (slot: string) => {
				const subject = grpc ? name : name.replace(SLOT, slot);
				return notAfter <= now
					? titles.expired(subject)
					: titles.expiresIn(subject, Math.ceil((notAfter - now) / DAY_MS));
			};
			return { build, link: grpc ? null : hostLink };
		}
		default:
			return { build: () => alert.title, link: null };
	}
}

function detailOf(alert: AlertDto) {
	const { facts } = alert;
	const details = text.details;
	switch (alert.type) {
		case "backup_failed":
			return details.backupFailed(
				alert.policy ?? text.unknownPolicy,
				facts.error,
			);
		case "no_recent_backup":
			return details.noRecent(
				relative(facts.last_success_at),
				facts.limit_secs,
			);
		case "agent_offline":
			return details.offline(relative(facts.last_seen_at));
		case "agent_update_failed":
			return details.agentUpdateFailed(facts.error);
		case "check_failed":
			return details.checkFailed(facts.findings ?? 0);
		default:
			return alert.detail;
	}
}

function AlertRow({ alert, now }: { alert: AlertDto; now: number }) {
	const { build, link } = titleOf(alert, now);
	const [before, after = ""] = build(SLOT).split(SLOT);
	const Icon = alert.severity === "critical" ? CircleX : TriangleAlert;
	const tone = alert.silenced
		? "text-neutral"
		: alert.severity === "critical"
			? "text-destructive"
			: "text-warning";
	return (
		<Item role="listitem" size="sm" className="items-start gap-2 px-0 py-2">
			<ItemMedia variant="icon" className={cn("mt-1", tone)}>
				<Icon aria-hidden />
			</ItemMedia>
			<ItemContent className="min-w-56">
				<ItemTitle
					className={cn(
						"line-clamp-none w-auto text-base wrap-anywhere",
						alert.silenced && "text-muted-foreground",
					)}
				>
					<span>
						{before}
						{link}
						{after}
					</span>
				</ItemTitle>
				<ItemDescription className="line-clamp-none wrap-anywhere">
					<TextWithPaths text={detailOf(alert)} />
				</ItemDescription>
			</ItemContent>
			<ItemActions className="ml-auto flex-wrap justify-end">
				<span className="text-sm text-muted-foreground">
					{text.since}
					<RelativeTime value={new Date(alert.started_at)} />
				</span>
				{alert.silenced && (
					<StatusBadge status="silenced">
						{alert.silenced_until == null
							? text.silence.badgeForever
							: text.silence.badge(formatDateTime(alert.silenced_until))}
					</StatusBadge>
				)}
			</ItemActions>
		</Item>
	);
}

export function AlertAnnouncer({ ids }: { ids?: string[] }) {
	const seen = useRef<Set<string> | null>(null);
	const [announcement, setAnnouncement] = useState({ count: 0, stamp: 0 });

	useEffect(() => {
		if (ids === undefined) return;
		const known = seen.current;
		seen.current = new Set(ids);
		if (known === null) return;
		const count = ids.filter((id) => !known.has(id)).length;
		setAnnouncement((prev) => ({
			count,
			stamp: count > 0 ? prev.stamp + 1 : prev.stamp,
		}));
	}, [ids]);

	return (
		<div role="status" aria-live="polite" className="sr-only">
			{announcement.count > 0 && (
				<span key={announcement.stamp}>
					{text.newAlerts(announcement.count)}
				</span>
			)}
		</div>
	);
}

export function OtherAlerts({ alerts }: { alerts: AlertDto[] }) {
	const headingId = useId();
	if (alerts.length === 0) return null;
	const now = Date.now();
	return (
		<section aria-labelledby={headingId} className="flex flex-col gap-2">
			<div className="flex items-center gap-2">
				<h2 id={headingId} className="text-sm font-semibold">
					{text.otherTitle}
				</h2>
				<Badge variant="outline" className="tabular-nums">
					{alerts.length}
				</Badge>
			</div>
			<div className="rounded-lg border bg-card px-4 py-2">
				<ItemGroup aria-labelledby={headingId} className="gap-2">
					{[...alerts].sort(order).map((alert) => (
						<AlertRow key={alert.id} alert={alert} now={now} />
					))}
				</ItemGroup>
			</div>
		</section>
	);
}

export function ActiveAlerts({
	hostId,
	title,
}: {
	hostId: string;
	title: string;
}) {
	const query = useListAlerts(
		{ state: "active", limit: MAX, host_id: hostId },
		{
			query: {
				refetchInterval: REFRESH_MS,
				refetchIntervalInBackground: false,
			},
		},
	);
	const [open, setOpen] = useState(false);
	const listId = useId();
	const headingId = useId();
	const { data } = query;
	const ids = useMemo(() => data?.alerts.map((a) => a.id), [data]);

	if (query.isPending) return <Skeleton className="h-16" />;
	if (query.isError && !data) {
		return (
			<Alert variant="destructive">
				<AlertDescription className="flex flex-wrap items-center gap-4">
					{text.loadFailed}
					<Button variant="outline" size="sm" onClick={() => query.refetch()}>
						{copy.overview.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}
	if (!data) return null;
	if (data.total === 0) return <AlertAnnouncer ids={ids} />;

	const alerts = [...data.alerts].sort(order).slice(0, MAX);
	const shown = open ? alerts : alerts.slice(0, SHOWN);
	const hidden = Math.max(0, data.total - alerts.length);
	const now = Date.now();

	return (
		<div className="contents">
			<AlertAnnouncer ids={ids} />
			<Card size="sm" className="px-4">
				<div className="flex items-center gap-2">
					<h2 id={headingId} className="text-base font-semibold">
						{title}
					</h2>
					<Badge variant="outline" className="tabular-nums">
						{data.total}
					</Badge>
				</div>
				<ItemGroup id={listId} aria-labelledby={headingId} className="gap-2">
					{shown.map((alert) => (
						<AlertRow key={alert.id} alert={alert} now={now} />
					))}
				</ItemGroup>
				{open && hidden > 0 && (
					<p className="text-sm text-muted-foreground">{text.more(hidden)}</p>
				)}
				{data.total > SHOWN && (
					<Button
						variant="outline"
						size="sm"
						aria-expanded={open}
						aria-controls={listId}
						className="self-start"
						onClick={() => setOpen(!open)}
					>
						{open ? text.showFewer : text.showAll(data.total)}
						<ChevronDown aria-hidden className={cn(open && "rotate-180")} />
					</Button>
				)}
			</Card>
		</div>
	);
}
