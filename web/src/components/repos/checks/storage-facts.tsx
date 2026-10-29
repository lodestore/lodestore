import type { RepoUsageDto } from "@/api/gen/model";
import { Fact } from "@/components/fact";
import { RelativeTime } from "@/components/relative-time";
import { copy } from "@/copy";
import { formatBytes, formatRatio } from "@/lib/format";

const text = copy.checks.storage;

export function StorageFacts({ usage }: { usage?: RepoUsageDto | null }) {
	if (!usage) {
		return <p className="text-sm text-muted-foreground">{text.none}</p>;
	}
	return (
		<dl className="flex flex-wrap gap-x-12 gap-y-4">
			<Fact label={text.used}>
				<span className="tabular-nums">
					{formatBytes(usage.physical_bytes)}
				</span>
			</Fact>
			<Fact label={text.logical}>
				<span className="tabular-nums">{formatBytes(usage.logical_bytes)}</span>
			</Fact>
			<Fact label={text.dedup}>
				<span className="tabular-nums">
					{usage.dedup_ratio != null
						? formatRatio(usage.dedup_ratio)
						: copy.common.dash}
				</span>
			</Fact>
			<Fact label={text.counted}>
				<RelativeTime value={new Date(usage.counted_at)} />
			</Fact>
		</dl>
	);
}
