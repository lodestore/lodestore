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

function HostRow({
	host,
	warnings,
	muted = false,
}: {
	host: MatchHost;
	warnings: HookWarning[];
	muted?: boolean;
}) {
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
					<StatusBadge status={host.online ? "online" : "offline"} />
				)}
			</div>
			{warnings.length > 0 && <Warnings warnings={warnings} />}
		</li>
	);
}

function HostList({
	hosts,
	warnings,
	muted,
}: {
	hosts: MatchHost[];
	warnings: HookWarning[];
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
	onRetry,
}: {
	notice?: string;
	matching?: Matching;
	warnings: HookWarning[];
	selectorEmpty: boolean;
	invalid: boolean;
	refreshing: boolean;
	failed: boolean;
	onRetry: () => void;
}) {
	const text = copy.policies.matching;
	const [open, setOpen] = useState(false);
	const own = matching?.hosts ?? [];
	const other = matching?.other_repository ?? [];
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
					text.summary(own.length),
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
			<Collapsible open={open && expandable} onOpenChange={setOpen}>
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
				<CollapsibleContent className="grid gap-6 px-(--card-spacing) pt-4 @min-[960px]:grid-cols-2">
					<div className="flex min-w-0 flex-col gap-4">
						{selectorEmpty && (
							<p className="text-sm text-muted-foreground">
								{text.emptySelector}
							</p>
						)}
						{own.length > 0 && <HostList hosts={own} warnings={warnings} />}
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
