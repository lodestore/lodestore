import { Link } from "@tanstack/react-router";
import { TriangleAlert } from "lucide-react";
import { useState } from "react";
import type { HookWarning, MatchHost, Matching } from "@/api/gen/model";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
	return warnings.map((warning) => {
		const text = warningText(warning);
		return (
			<Tooltip key={`${warning.kind}-${warning.script ?? ""}`}>
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
	const own = matching?.hosts ?? [];
	const other = matching?.other_repository ?? [];

	let body: React.ReactNode = null;
	if (failed) {
		body = (
			<Alert variant="destructive" role="alert">
				<AlertDescription>
					{text.loadFailed}{" "}
					<Button variant="link" className="h-auto p-0" onClick={onRetry}>
						{copy.errors.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	} else if (notice) {
		body = <p className="text-sm text-muted-foreground">{notice}</p>;
	} else if (invalid) {
		body = <p className="text-sm text-muted-foreground">{text.invalid}</p>;
	} else if (matching) {
		body = (
			<>
				<div className="flex flex-col gap-1 text-sm">
					<p className="font-semibold">{text.summary(own.length)}</p>
					{other.length > 0 && (
						<p className="text-muted-foreground">
							{text.skipped(other.length)}
						</p>
					)}
					{selectorEmpty && (
						<p className="text-muted-foreground">{text.emptySelector}</p>
					)}
				</div>
				{own.length === 0 && other.length === 0 && (
					<p className="text-sm text-muted-foreground">{text.zero}</p>
				)}
				{own.length > 0 && <HostList hosts={own} warnings={warnings} />}
				{other.length > 0 && (
					<section className="flex flex-col gap-1">
						<h3 className="text-sm font-semibold">{text.otherRepo}</h3>
						<HostList muted hosts={other} warnings={[]} />
					</section>
				)}
			</>
		);
	}

	return (
		<Card className="@container">
			<CardHeader className="flex items-center justify-between">
				<CardTitle>
					<h2>{text.title}</h2>
				</CardTitle>
				{refreshing && <Spinner className="size-4" />}
			</CardHeader>
			<CardContent className="flex flex-col gap-4">{body}</CardContent>
		</Card>
	);
}
