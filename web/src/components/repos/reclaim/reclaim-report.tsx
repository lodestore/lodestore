import {
	ChevronDown,
	CircleAlert,
	Download,
	Info,
	Lock,
	TriangleAlert,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { getReport } from "@/api/gen/endpoints";
import type { GcEventDto, GcReport } from "@/api/gen/model";
import { Fact } from "@/components/fact";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Spinner } from "@/components/ui/spinner";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";
import {
	formatBytes,
	formatCount,
	formatDateTime,
	formatDuration,
	formatShortDateTime,
} from "@/lib/format";

const text = copy.gc.report;

function saveJson(name: string, value: unknown) {
	const url = URL.createObjectURL(
		new Blob([JSON.stringify(value, null, 2)], { type: "application/json" }),
	);
	const link = document.createElement("a");
	link.href = url;
	link.download = name;
	link.click();
	URL.revokeObjectURL(url);
}

function ReportBody({
	repositoryId,
	report,
}: {
	repositoryId: string;
	report: GcReport;
}) {
	const [downloading, setDownloading] = useState(false);
	const dry = report.dry_run;
	const freed = report.bytes_freed;
	const locked = report.locked_bytes;

	async function download() {
		setDownloading(true);
		try {
			saveJson(
				`cleanup-report-${report.cycle_id}.json`,
				await getReport(repositoryId, report.cycle_id),
			);
		} catch {
			toast.error(text.downloadFailed);
		} finally {
			setDownloading(false);
		}
	}

	const summary = dry
		? freed > 0
			? text.wouldFree(formatBytes(freed))
			: text.nothingToFree
		: freed > 0
			? text.freed(formatBytes(freed))
			: text.nothingFreed;

	return (
		<div className="flex flex-col gap-4">
			<div className="flex flex-col gap-1">
				<p className="font-semibold wrap-anywhere">
					{summary}
					{locked > 0 && ` ${text.locked(formatBytes(locked))}`}
				</p>
				<p className="text-sm text-muted-foreground">
					{text.finished}
					<RelativeTime value={new Date(report.finished_at)} />
					{!dry && text.took(formatDuration(report.duration_ms))}
				</p>
			</div>
			<dl className="flex flex-wrap gap-x-12 gap-y-4">
				<Fact label={dry ? text.factWouldFree : text.factFreed}>
					<span className="tabular-nums">{formatBytes(freed)}</span>
				</Fact>
				<Fact label={dry ? text.factToDelete : text.factDeleted}>
					<span className="tabular-nums">
						{formatCount(report.packs_deleted)}
					</span>
				</Fact>
				<Fact label={dry ? text.factToRepack : text.factRepacked}>
					<span className="tabular-nums">
						{formatCount(report.packs_repacked)}
					</span>
				</Fact>
				{report.packs_repacked > 0 && (
					<Fact label={text.factTransfer}>
						<span className="tabular-nums">
							{formatBytes(report.repack_transfer_bytes)}
						</span>
					</Fact>
				)}
				{locked > 0 && (
					<Fact label={text.factLocked}>
						<span className="tabular-nums">{formatBytes(locked)}</span>
						<span className="flex items-center gap-1 text-sm font-normal text-muted-foreground">
							<Lock aria-hidden className="size-4" />
							{report.locked_until != null && !report.locked_until_unknown
								? text.freesAfter(formatDateTime(report.locked_until))
								: text.dateUnknown}
						</span>
					</Fact>
				)}
			</dl>
			<Button
				type="button"
				variant="outline"
				size="sm"
				className="w-fit aria-disabled:opacity-50"
				aria-disabled={downloading || undefined}
				onClick={() => {
					if (!downloading) void download();
				}}
			>
				{downloading ? <Spinner /> : <Download aria-hidden />}
				{text.download}
			</Button>
		</div>
	);
}

export function ReclaimReport({
	repositoryId,
	cleanup,
	dryRun,
	busy,
}: {
	repositoryId: string;
	cleanup?: GcReport | null;
	dryRun?: GcReport | null;
	busy?: boolean;
}) {
	const newer =
		cleanup && dryRun && dryRun.finished_at > cleanup.finished_at
			? "dry-run"
			: "cleanup";
	const latestId = (newer === "dry-run" ? dryRun : cleanup)?.cycle_id;
	const [picked, setPicked] = useState<{ latest?: string; tab: string }>({
		tab: newer,
	});
	const tab = picked.latest === latestId ? picked.tab : newer;
	return (
		<section
			aria-busy={busy || undefined}
			className="flex flex-col gap-4 rounded-lg bg-muted p-4"
		>
			{cleanup && dryRun ? (
				<Tabs
					value={tab}
					onValueChange={(next) => setPicked({ latest: latestId, tab: next })}
				>
					<TabsList variant="line">
						<TabsTrigger value="cleanup">{text.lastCleanup}</TabsTrigger>
						<TabsTrigger value="dry-run">{text.lastDryRun}</TabsTrigger>
					</TabsList>
					<TabsContent value="cleanup">
						<ReportBody repositoryId={repositoryId} report={cleanup} />
					</TabsContent>
					<TabsContent value="dry-run">
						<ReportBody repositoryId={repositoryId} report={dryRun} />
					</TabsContent>
				</Tabs>
			) : cleanup || dryRun ? (
				<>
					<h3 className="text-sm font-semibold">
						{cleanup ? text.lastCleanup : text.lastDryRun}
					</h3>
					<ReportBody
						repositoryId={repositoryId}
						report={(cleanup ?? dryRun) as GcReport}
					/>
				</>
			) : (
				<p className="text-sm text-muted-foreground">{text.empty}</p>
			)}
		</section>
	);
}

const LOG_SHOWN = 20;

function LevelIcon({ level }: { level: string }) {
	if (level === "warning") {
		return (
			<TriangleAlert aria-hidden className="size-4 shrink-0 text-warning" />
		);
	}
	if (level === "error") {
		return (
			<CircleAlert aria-hidden className="size-4 shrink-0 text-destructive" />
		);
	}
	return <Info aria-hidden className="size-4 shrink-0" />;
}

export function CycleLog({
	events,
	total,
}: {
	events: GcEventDto[];
	total: number;
}) {
	const t = copy.gc.log;
	const shown = events.slice(-LOG_SHOWN).reverse();
	const more = Math.max(total, events.length) - shown.length;
	return (
		<Collapsible className="flex flex-col gap-4">
			<CollapsibleTrigger className="group flex w-fit items-center gap-1 text-sm font-semibold outline-none focus-visible:ring-2 focus-visible:ring-ring">
				<ChevronDown
					aria-hidden
					className="size-4 transition-transform group-data-[state=closed]:-rotate-90"
				/>
				{t.title}
			</CollapsibleTrigger>
			<CollapsibleContent>
				<ul className="flex flex-col gap-2">
					{shown.map((event) => (
						<li key={event.seq} className="flex items-start gap-2 text-sm">
							<time
								dateTime={new Date(event.at).toISOString()}
								className="shrink-0 tabular-nums text-muted-foreground"
							>
								{formatShortDateTime(event.at)}
							</time>
							<LevelIcon level={event.level} />
							<span className="sr-only">
								{t.level[event.level as keyof typeof t.level] ?? t.level.info}
							</span>
							<span className="min-w-0 wrap-anywhere">
								<TextWithPaths text={event.message} />
							</span>
						</li>
					))}
					{more > 0 && (
						<li className="text-sm text-muted-foreground">{t.more(more)}</li>
					)}
				</ul>
			</CollapsibleContent>
		</Collapsible>
	);
}
