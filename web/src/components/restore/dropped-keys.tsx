import { Ban, Check, ChevronDown, type LucideIcon } from "lucide-react";
import { useState } from "react";
import type { DroppedKey } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { copy } from "@/copy";
import { displayName } from "@/lib/names";

const text = copy.restore.container;
const VISIBLE = 6;
const REASONS: Record<string, string | undefined> = text.reasons;

export const droppedReason = (reason: string) =>
	REASONS[reason] ?? text.reasons.unknown;

/** Lines `key: value` with an optional note under each; the dialog and the job show config keys the same way. */
export function KeyRows({
	rows,
	icon: Icon,
}: {
	rows: { key: string; value: string; note?: string }[];
	icon: LucideIcon;
}) {
	return (
		<ul className="flex flex-col divide-y">
			{rows.map((row) => (
				<li key={`${row.key}:${row.value}`} className="flex gap-2 py-2">
					<Icon
						aria-hidden
						className="mt-1 size-4 shrink-0 text-muted-foreground"
					/>
					<div className="flex min-w-0 flex-col">
						<span className="font-mono text-sm wrap-anywhere">
							{displayName(row.value ? `${row.key}: ${row.value}` : row.key)}
						</span>
						{row.note && (
							<span className="text-sm text-muted-foreground">{row.note}</span>
						)}
					</div>
				</li>
			))}
		</ul>
	);
}

export function DroppedKeys({
	dropped,
	collapsed = false,
}: {
	dropped: DroppedKey[];
	collapsed?: boolean;
}) {
	const [open, setOpen] = useState(false);
	if (dropped.length === 0) {
		return (
			<p className="flex items-center gap-2 text-sm">
				<Check aria-hidden className="size-4 shrink-0 text-muted-foreground" />
				{text.droppedNone}
			</p>
		);
	}
	const rows = dropped.map((d) => ({
		key: d.key,
		value: d.value,
		note: droppedReason(d.reason),
	}));
	const shown = collapsed ? 0 : VISIBLE;
	const hidden = rows.length - shown;
	return (
		<Collapsible open={open} onOpenChange={setOpen}>
			{collapsed && (
				<p className="font-mono text-sm wrap-anywhere">
					{dropped.map((d) => d.key).join(", ")}
				</p>
			)}
			<KeyRows rows={rows.slice(0, shown)} icon={Ban} />
			{hidden > 0 && (
				<>
					<CollapsibleContent>
						<KeyRows rows={rows.slice(shown)} icon={Ban} />
					</CollapsibleContent>
					<CollapsibleTrigger asChild>
						<Button
							type="button"
							variant="outline"
							size="sm"
							className="mt-2 self-start"
						>
							<ChevronDown
								aria-hidden
								className={open ? "rotate-180" : undefined}
							/>
							{open
								? text.droppedHide
								: collapsed
									? text.droppedShow
									: text.droppedMore(hidden)}
						</Button>
					</CollapsibleTrigger>
				</>
			)}
		</Collapsible>
	);
}
