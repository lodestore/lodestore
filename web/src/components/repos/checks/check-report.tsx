import { CircleX, Download } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { getCheckReport } from "@/api/gen/endpoints";
import type { CheckRunDto, PackCoverage } from "@/api/gen/model";
import { Fact } from "@/components/fact";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatBytes, formatCount, formatDuration } from "@/lib/format";

const text = copy.checks.report;

const SHOWN = 20;
const COVERAGE_DAYS = 30;

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

function Count({ value }: { value: number }) {
	return <span className="tabular-nums">{formatCount(value)}</span>;
}

function Facts({
	run,
	coverage,
}: {
	run: CheckRunDto;
	coverage?: PackCoverage | null;
}) {
	const t = text.facts;
	const structure = run.summary.structure;
	const packs = run.summary.packs;
	const restore = run.summary.restore;
	return (
		<dl className="flex flex-wrap gap-x-12 gap-y-4">
			{structure && (
				<>
					<Fact label={t.snapshotsChecked}>
						<Count value={structure.snapshots_checked} />
					</Fact>
					<Fact label={t.treesChecked}>
						<Count value={structure.trees_checked} />
					</Fact>
					<Fact label={t.packsListed}>
						<Count value={structure.packs_listed} />
					</Fact>
					<Fact label={t.missingPacks}>
						<Count value={structure.missing_packs} />
					</Fact>
				</>
			)}
			{packs && (
				<>
					<Fact label={t.packsRead}>
						<Count value={packs.packs_read} />
					</Fact>
					<Fact label={t.dataRead}>
						<span className="tabular-nums">
							{formatBytes(packs.bytes_read)}
						</span>
					</Fact>
					<Fact label={t.blobsVerified}>
						<Count value={packs.blobs_verified} />
					</Fact>
					<Fact label={t.damagedBlobs}>
						<Count value={packs.damaged_blobs} />
					</Fact>
					{coverage && (
						<Fact label={t.coverage(COVERAGE_DAYS)}>
							<span className="tabular-nums">
								{formatCount(Math.round(coverage.checked_30d_pct))}%
							</span>
							{coverage.never_checked > 0 && (
								<span className="text-sm font-normal text-muted-foreground">
									{text.neverChecked(coverage.never_checked)}
								</span>
							)}
						</Fact>
					)}
				</>
			)}
			{restore && (
				<>
					<Fact label={t.snapshotsSampled}>
						<Count value={restore.snapshots_sampled} />
					</Fact>
					<Fact label={t.filesRestored}>
						<Count value={restore.files_restored} />
					</Fact>
					<Fact label={t.dataRestored}>
						<span className="tabular-nums">
							{formatBytes(restore.bytes_restored)}
						</span>
					</Fact>
					<Fact label={t.hashMismatches}>
						<Count value={restore.hash_mismatches} />
					</Fact>
					<Fact label={t.skipped}>
						<Count value={restore.skipped} />
						{restore.stopped_at_limit && (
							<span className="block text-sm font-normal text-muted-foreground">
								{text.stoppedAtLimit}
							</span>
						)}
					</Fact>
				</>
			)}
		</dl>
	);
}

export function CheckReport({
	repositoryId,
	run,
	coverage,
}: {
	repositoryId: string;
	run: CheckRunDto;
	coverage?: PackCoverage | null;
}) {
	const [downloading, setDownloading] = useState(false);
	const stopped =
		run.outcome === "stopped" ||
		(run.state === "failed" && run.outcome !== "problems");
	const shown = run.findings.slice(0, SHOWN);
	const more = Math.max(run.findings_total, run.findings.length) - shown.length;
	const summary = stopped
		? text.stopped(run.error ?? "")
		: run.findings_total > 0
			? text.problems(run.findings_total)
			: text.passed;

	async function download() {
		setDownloading(true);
		try {
			saveJson(
				`check-report-${run.id}.json`,
				await getCheckReport(repositoryId, run.id),
			);
		} catch {
			toast.error(text.downloadFailed);
		} finally {
			setDownloading(false);
		}
	}

	return (
		<section className="flex flex-col gap-4 rounded-lg bg-muted p-4">
			<div className="flex flex-col gap-1">
				<p className="font-semibold wrap-anywhere">
					<TextWithPaths text={summary} />
				</p>
				{run.finished_at != null && (
					<p className="text-sm text-muted-foreground">
						{text.finished}
						<RelativeTime value={new Date(run.finished_at)} />
						{run.duration_ms != null &&
							text.took(formatDuration(run.duration_ms))}
					</p>
				)}
			</div>
			<Facts run={run} coverage={coverage} />
			{shown.length > 0 && (
				<ul className="flex flex-col gap-2">
					{shown.map((finding, i) => (
						<li
							key={`${finding.key}:${i}`}
							className="flex items-start gap-2 text-sm"
						>
							<CircleX
								aria-hidden
								className="size-4 shrink-0 text-destructive"
							/>
							<span className="sr-only">{text.findingLabel}</span>
							<span className="min-w-0 wrap-anywhere">
								<TextWithPaths
									text={
										finding.detail.includes(finding.key)
											? finding.detail
											: `${finding.detail} ${finding.key}`
									}
								/>
							</span>
						</li>
					))}
					{more > 0 && (
						<li className="text-sm text-muted-foreground">{text.more(more)}</li>
					)}
				</ul>
			)}
			{run.findings_total > 0 && (
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
			)}
		</section>
	);
}
