import { keepPreviousData } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Info } from "lucide-react";
import { useHostSources } from "@/api/gen/endpoints";
import type { HostDetail, SourceItem } from "@/api/gen/model";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Truncated } from "@/components/truncated";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { type SourceType, sourceTypeIcon } from "@/lib/source-type";

const REFRESH_MS = 15_000;
const FROM_680 = "hidden @min-[680px]:table-cell";
const PAD = "px-2 @min-[680px]:px-4";
const text = copy.sources.host;

function TypeIcon({ type }: { type: string }) {
	const Icon = sourceTypeIcon[type as SourceType] ?? sourceTypeIcon.files;
	return <Icon aria-hidden className="size-4 shrink-0 text-muted-foreground" />;
}

function SourceCell({ type, label }: { type: string; label: string }) {
	return (
		<div className="flex max-w-48 items-center gap-2 @min-[680px]:max-w-72">
			<TypeIcon type={type} />
			<Truncated text={label} className="min-w-0" />
		</div>
	);
}

function PolicyLinks({ policies }: { policies: string[] }) {
	const [first, ...rest] = [...policies].sort((a, b) => a.localeCompare(b));
	return (
		<span className="flex items-center gap-1">
			<Link
				to="/policies/$name"
				params={{ name: first }}
				className="font-semibold text-primary-strong underline-offset-4 hover:underline"
			>
				{first}
			</Link>
			{rest.length > 0 && (
				<Tooltip>
					<TooltipTrigger asChild>
						<span className="text-muted-foreground">
							{text.more(rest.length)}
						</span>
					</TooltipTrigger>
					<TooltipContent>{[first, ...rest].join(", ")}</TooltipContent>
				</Tooltip>
			)}
		</span>
	);
}

function Protection({ protection }: Pick<SourceItem, "protection">) {
	if (protection.state === "agent_too_old") {
		return (
			<Badge variant="outline" className="gap-1 text-neutral">
				<Info aria-hidden />
				{text.protection.agentTooOld}
			</Badge>
		);
	}
	if (protection.state === "protected" && protection.policies.length > 0) {
		return <PolicyLinks policies={protection.policies} />;
	}
	return (
		<StatusBadge status="warning">{text.protection.notProtected}</StatusBadge>
	);
}

function LastSnapshot({ at }: { at: number | null | undefined }) {
	return at == null ? (
		<span className="text-muted-foreground">{text.never}</span>
	) : (
		<RelativeTime value={new Date(at)} tabIndex={-1} />
	);
}

function details(item: SourceItem) {
	const parts = [item.pool, ...item.tags].filter(Boolean);
	return parts.length > 0 ? parts.join(" · ") : copy.common.dash;
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-12">
			<TableCell className={PAD}>
				<Skeleton className="h-4 w-32" />
			</TableCell>
			<TableCell className={`${PAD} ${FROM_680}`}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={PAD}>
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell className={`${PAD} ${FROM_680}`}>
				<Skeleton className="h-4 w-16" />
			</TableCell>
		</TableRow>
	));
}

export function HostSources({ host }: { host: HostDetail }) {
	const query = useHostSources(host.id, {
		query: {
			placeholderData: keepPreviousData,
			refetchInterval: REFRESH_MS,
			refetchIntervalInBackground: false,
		},
	});
	const data = query.data;

	if (data === undefined && query.isError) {
		return (
			<Alert variant="destructive" role="alert" className="mt-4">
				<AlertDescription className="flex items-center justify-between gap-4">
					{text.loadFailed}
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={() => query.refetch()}
					>
						{text.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}

	return (
		<div className="flex flex-col gap-4 pt-4">
			<div className="@container overflow-hidden rounded-lg border bg-card">
				<Table>
					<TableHeader className="bg-muted">
						<TableRow>
							<TableHead className={PAD}>{text.columns.source}</TableHead>
							<TableHead className={`${PAD} ${FROM_680}`}>
								{text.columns.poolTags}
							</TableHead>
							<TableHead className={PAD}>{text.columns.protection}</TableHead>
							<TableHead className={`${PAD} ${FROM_680}`}>
								{text.columns.lastSnapshot}
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{data === undefined ? (
							<SkeletonRows />
						) : (
							data.items.map((item, i) => (
								<TableRow key={i} className="h-12">
									<TableCell className={`${PAD} text-sm font-semibold`}>
										<SourceCell type={item.source_type} label={item.label} />
									</TableCell>
									<TableCell
										className={`${PAD} ${FROM_680} text-sm text-muted-foreground`}
									>
										{details(item)}
									</TableCell>
									<TableCell className={`${PAD} text-sm`}>
										<Protection protection={item.protection} />
									</TableCell>
									<TableCell className={`${PAD} ${FROM_680} text-sm`}>
										<LastSnapshot at={item.last_snapshot_at} />
									</TableCell>
								</TableRow>
							))
						)}
					</TableBody>
				</Table>
			</div>
		</div>
	);
}
