import { Info, TriangleAlert } from "lucide-react";
import type { FindGroup } from "@/api/gen/model";
import { PathText } from "@/components/path-text";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Card } from "@/components/ui/card";
import { copy } from "@/copy";
import { displayName } from "@/lib/names";
import { formatMonthDay, formatMonthDayTime } from "./dates";
import { VersionEntry } from "./version-entry";

function PathHeading({ path }: { path: string }) {
	const cut = path.lastIndexOf("/") + 1;
	return (
		<h2 tabIndex={-1} className="font-mono text-base outline-none">
			<PathText value={path.slice(0, cut)} />
			<span className="font-semibold">{path.slice(cut)}</span>
		</h2>
	);
}

export function FindDetail({
	sessionId,
	group,
}: {
	sessionId: string;
	group: FindGroup;
}) {
	const sameDay =
		group.gone != null &&
		formatMonthDay(group.gone.last_ms) === formatMonthDay(group.gone.next_ms);
	const goneDate = sameDay ? formatMonthDayTime : formatMonthDay;
	return (
		<Card className="min-w-0 px-6">
			<div className="flex flex-col gap-1">
				<p className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
					<span className="font-semibold wrap-anywhere">
						{displayName(group.hostname)}
					</span>
					{group.labels.length > 0 && (
						<span className="font-mono text-sm text-muted-foreground">
							{group.labels
								.map((label) => displayName(`${label.key}=${label.value}`))
								.join(" ")}
						</span>
					)}
				</p>
				<PathHeading path={displayName(group.path)} />
			</div>
			{group.gone != null && (
				<Alert role="status" className="border-warning/60">
					<TriangleAlert aria-hidden className="text-warning" />
					<AlertDescription>
						<strong className="font-semibold">
							{copy.find.gone.heading(goneDate(group.gone.last_ms))}
						</strong>{" "}
						{copy.find.gone.body(goneDate(group.gone.next_ms))}
					</AlertDescription>
				</Alert>
			)}
			{!group.agent && (
				<Alert role="status">
					<Info aria-hidden />
					<AlertDescription>
						{copy.restore.noAgent(group.hostname)}
					</AlertDescription>
				</Alert>
			)}
			<ol className="flex flex-col">
				{group.versions.map((version, i) => (
					<VersionEntry
						key={`${version.first_ms}-${version.content_id}`}
						sessionId={sessionId}
						group={group}
						version={version}
						newest={i === 0}
					/>
				))}
			</ol>
		</Card>
	);
}
