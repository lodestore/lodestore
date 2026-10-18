import { CircleSlash, ShieldAlert } from "lucide-react";
import type { SnapshotItem } from "@/api/gen/model";
import { Badge } from "@/components/ui/badge";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { formatBytes, formatCount } from "@/lib/format";

const text = copy.snapshots;

const times = (factor: number) => String(Number(factor.toFixed(1)));

export function suspicionReasons({ suspicious }: SnapshotItem): string[] {
	if (!suspicious) return [];
	if (!suspicious.automatic) {
		return [text.markedBy(suspicious.marked_by, suspicious.note ?? "")];
	}
	return suspicious.reasons.map(({ kind, pct, from, to, factor }) => {
		switch (kind) {
			case "files_drop":
				return text.reasons.files_drop(
					String(pct),
					formatCount(from),
					formatCount(to),
				);
			case "bytes_drop":
				return text.reasons.bytes_drop(
					String(pct),
					formatBytes(from),
					formatBytes(to),
				);
			case "changed_spike":
				return text.reasons.changed_spike(String(pct), times(factor));
			case "added_spike":
				return text.reasons.added_spike(formatBytes(to), times(factor));
			default:
				return text.reasons.unknown;
		}
	});
}

export function SuspiciousBadge({
	snapshot,
	plain = false,
}: {
	snapshot: SnapshotItem;
	plain?: boolean;
}) {
	const reasons = suspicionReasons(snapshot);
	const badge = (
		<Badge
			variant="outline"
			data-status="suspicious"
			className="gap-1 text-warning"
			{...(plain
				? { title: reasons.join(" ") || undefined }
				: {
						role: "group",
						tabIndex: 0,
						"aria-label": text.suspiciousLabel(reasons.join(" ")),
					})}
		>
			<ShieldAlert aria-hidden />
			<span>{text.suspicious}</span>
		</Badge>
	);
	if (plain) return badge;
	return (
		<Tooltip>
			<TooltipTrigger asChild>{badge}</TooltipTrigger>
			<TooltipContent className="max-w-sm wrap-anywhere">
				<ul className="flex flex-col gap-1">
					{reasons.map((reason, index) => (
						<li key={`${index}-${reason}`}>{reason}</li>
					))}
				</ul>
			</TooltipContent>
		</Tooltip>
	);
}

export function PolicyDeletedBadge() {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<Badge
					variant="outline"
					data-status="policy-deleted"
					tabIndex={0}
					className="gap-1 text-neutral"
				>
					<CircleSlash aria-hidden />
					<span>{text.policyDeleted}</span>
				</Badge>
			</TooltipTrigger>
			<TooltipContent className="max-w-sm">
				{text.policyDeletedTip}
			</TooltipContent>
		</Tooltip>
	);
}
