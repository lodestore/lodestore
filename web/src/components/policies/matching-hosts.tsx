import { Link } from "@tanstack/react-router";
import { ChevronDown, ChevronUp, TriangleAlert, Users } from "lucide-react";
import { useState } from "react";
import type { HookWarning, MatchHost, Matching } from "@/api/gen/model";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Spinner } from "@/components/ui/spinner";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { sourceTypeNoun } from "@/lib/source-type";
import { cn } from "@/lib/utils";

const VISIBLE = 10;

export function warningText(warning: HookWarning) {
	const script = warning.script ?? "";
	switch (warning.kind) {
		case "script_missing":
			return copy.policies.matching.hookMissing(script);
		case "script_rejected":
			return copy.policies.matching.hookRejected(script);
		case "inline_disabled":
			return copy.policies.matching.inlineOff;
	}
}

function Warnings({ warnings }: { warnings: HookWarning[] }) {
	return warnings.map((warning, index) => {
		const text = warningText(warning);
		return (
			<Tooltip key={`${index}-${warning.kind}-${warning.script ?? ""}`}>
				<TooltipTrigger asChild>
					<span
						role="img"
						tabIndex={0}
						aria-label={copy.policies.matching.hookWarning(text)}
						className="flex min-w-0 items-center gap-1 text-sm text-warning"
					>
						<TriangleAlert aria-hidden className="size-4 shrink-0" />
						<span aria-hidden className="truncate">
							{text}
						</span>
					</span>
				</TooltipTrigger>
				<TooltipContent>{text}</TooltipContent>
			</Tooltip>
		);
	});
}

const isGuests = (type?: string) =>
	type === "proxmox-vm" || type === "proxmox-ct";

function instanceText(type: string | undefined, n: number | null | undefined) {
	if (n == null) return undefined;
	const text = copy.policies.matching.instances;
	if (isGuests(type)) return text.guest(n);
	if (type === "postgres") return text.database(n);
	if (type === "mysql") return text.schema(n);
	return undefined;
}

function markText(host: MatchHost, type: string | undefined) {
	if (type === undefined || type === "files") return undefined;
	const marks = copy.policies.matching.marks;
	switch (host.source_mark) {
		case "agent_too_old":
			return marks.agentTooOld(sourceTypeNoun(type ?? "") ?? "");
		case "not_found":
			if (isGuests(type)) return marks.notFound.proxmox;
			return type === "mysql" ? marks.notFound.mysql : marks.notFound.postgres;
		case "selects_nothing":
			if (isGuests(type)) return marks.selectsNothing.guests;
			return type === "mysql"
				? marks.selectsNothing.schemas
				: marks.selectsNothing.databases;
		default:
			return undefined;
	}
}

function HostRow({
	host,
	warnings,
	sourceType,
	muted = false,
}: {
	host: MatchHost;
	warnings: HookWarning[];
	sourceType?: string;
	muted?: boolean;
}) {
	const mark = muted ? undefined : markText(host, sourceType);
	const instances = muted
		? undefined
		: instanceText(sourceType, host.instances);
	return (
		<li className="flex flex-col gap-1 py-2">
			<div className="flex items-center justify-between gap-2">
				<Link
					to="/hosts/$id"
					params={{ id: host.id }}
					className={
						muted
							? "min-w-0 truncate text-muted-foreground underline-offset-4 hover:underline"
							: "min-w-0 truncate font-semibold text-primary-strong underline-offset-4 hover:underline"
					}
				>
					{host.hostname}
				</Link>
				{muted ? (
					<Badge variant="outline" className="shrink-0">
						{copy.policies.matching.usesRepo(host.repository_name)}
					</Badge>
				) : (
					<span className="flex shrink-0 items-center gap-2">
						{instances && (
							<span className="text-sm text-muted-foreground">{instances}</span>
						)}
						<StatusBadge status={host.online ? "online" : "offline"} />
					</span>
				)}
			</div>
			{mark && (
				<p className="flex items-start gap-1 text-sm text-warning">
					<TriangleAlert aria-hidden className="mt-1 size-4 shrink-0" />
					{mark}
				</p>
			)}
			{warnings.length > 0 && <Warnings warnings={warnings} />}
		</li>
	);
}

function HostList({
	hosts,
	warnings,
	sourceType,
	muted,
}: {
	hosts: MatchHost[];
	warnings: HookWarning[];
	sourceType?: string;
	muted?: boolean;
}) {
	const [all, setAll] = useState(false);
	const shown = all ? hosts : hosts.slice(0, VISIBLE);
	return (
		<>
			<ul className="flex flex-col divide-y">
				{shown.map((host) => (
					<HostRow
						key={host.id}
						host={host}
						muted={muted}
						sourceType={sourceType}
						warnings={warnings.filter((w) => w.host_id === host.id)}
					/>
				))}
			</ul>
			{hosts.length > VISIBLE && !all && (
				<Button
					type="button"
					variant="outline"
					size="sm"
					className="self-start"
					onClick={() => setAll(true)}
				>
					{copy.policies.matching.showAll(hosts.length)}
				</Button>
			)}
		</>
	);
}

export function MatchingHosts({
	matching,
	warnings,
	selectorEmpty,
	invalid,
	refreshing,
	failed,
	notice,
	sourceType,
	onRetry,
}: {
	notice?: string;
	sourceType?: string;
	matching?: Matching;
	warnings: HookWarning[];
	selectorEmpty: boolean;
	invalid: boolean;
	refreshing: boolean;
	failed: boolean;
	onRetry: () => void;
}) {
	const text = copy.policies.matching;
	const [chosen, setChosen] = useState<boolean>();
	const own = matching?.hosts ?? [];
	const other = matching?.other_repository ?? [];
	const failing = own.filter((host) => markText(host, sourceType)).length;
	const open = chosen ?? failing > 0;
	const expandable =
		!failed && !notice && !invalid && own.length + other.length > 0;

	let line: React.ReactNode = null;
	if (failed) {
		line = (
			<Alert variant="destructive" role="alert">
				<AlertDescription className="flex flex-wrap items-center justify-between gap-2">
					{text.loadFailed}
					<Button type="button" variant="outline" size="sm" onClick={onRetry}>
						{copy.errors.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	} else if (notice) {
		line = <p className="text-sm text-muted-foreground">{notice}</p>;
	} else if (invalid) {
		line = <p className="text-sm text-muted-foreground">{text.invalid}</p>;
	} else if (matching && !expandable) {
		line = <p className="text-sm text-muted-foreground">{text.zero}</p>;
	} else if (matching) {
		line = (
			<p className="min-w-0 text-sm font-semibold">
				{[
					failing > 0
						? text.matchSummary(own.length, failing)
						: text.summary(own.length),
					other.length > 0 ? text.skipped(other.length) : null,
				]
					.filter(Boolean)
					.join(text.separator)}
			</p>
		);
	}

	return (
		<Card size="sm" className="@container">
			<h2 className="sr-only">{text.title}</h2>
			<Collapsible open={open && expandable} onOpenChange={setChosen}>
				<div className="flex items-center gap-2 px-(--card-spacing)">
					{refreshing ? (
						<Spinner />
					) : (
						<Users
							aria-hidden
							className="size-4 shrink-0 text-muted-foreground"
						/>
					)}
					<div className="min-w-0 flex-1">{line}</div>
					{expandable && !open && failing > 0 && (
						<span className="flex shrink-0 items-center gap-1 text-sm text-warning">
							<TriangleAlert aria-hidden className="size-4" />
							{text.willFail(failing)}
						</span>
					)}
					{expandable && warnings.length > 0 && (
						<span className="flex shrink-0 items-center gap-1 text-sm text-warning">
							<TriangleAlert aria-hidden className="size-4" />
							{text.hookWarnings(warnings.length)}
						</span>
					)}
					{expandable && (
						<CollapsibleTrigger asChild>
							<Button
								type="button"
								variant="ghost"
								size="icon-sm"
								aria-label={open ? text.hide : text.show}
							>
								{open ? <ChevronUp aria-hidden /> : <ChevronDown aria-hidden />}
							</Button>
						</CollapsibleTrigger>
					)}
				</div>
				<CollapsibleContent
					className={cn(
						"grid gap-6 px-(--card-spacing) pt-4",
						other.length > 0 && "@min-[960px]:grid-cols-2",
					)}
				>
					<div className="flex min-w-0 flex-col gap-4">
						{selectorEmpty && (
							<p className="text-sm text-muted-foreground">
								{text.emptySelector}
							</p>
						)}
						{own.length > 0 && (
							<HostList
								hosts={own}
								warnings={warnings}
								sourceType={sourceType}
							/>
						)}
					</div>
					{other.length > 0 && (
						<section className="flex min-w-0 flex-col gap-1">
							<h3 className="text-sm font-semibold">{text.otherRepo}</h3>
							<HostList muted hosts={other} warnings={[]} />
						</section>
					)}
				</CollapsibleContent>
			</Collapsible>
		</Card>
	);
}
