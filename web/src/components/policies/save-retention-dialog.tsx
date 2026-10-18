import type { RetentionPreview } from "@/api/gen/model";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { hostCount, snapshotCount } from "@/lib/plural";
import { forgetTotals } from "./retention-preview";

const ROWS_SHOWN = 5;

export function SaveRetentionDialog({
	preview,
	onConfirm,
	onCancel,
}: {
	preview?: RetentionPreview;
	onConfirm: () => void;
	onCancel: () => void;
}) {
	const text = copy.policies.retention.confirm;
	const totals = preview && forgetTotals(preview);
	const rows = (preview?.hosts ?? []).flatMap((host) =>
		host.snapshots.map((snapshot) => ({ host, snapshot })),
	);
	const shown = rows.slice(0, ROWS_SHOWN);
	const more = (totals?.forget ?? 0) - shown.length;
	return (
		<AlertDialog
			open={preview !== undefined}
			onOpenChange={(open) => {
				if (!open) onCancel();
			}}
		>
			<AlertDialogContent>
				<AlertDialogHeader>
					<AlertDialogTitle>{text.title}</AlertDialogTitle>
					<AlertDialogDescription>
						{totals &&
							text.body(snapshotCount(totals.forget), hostCount(totals.hosts))}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{shown.length > 0 && (
					<div className="flex flex-col gap-1 rounded-lg bg-muted p-4 text-sm">
						<ul className="flex flex-col gap-1">
							{shown.map(({ host, snapshot }) => (
								<li
									key={snapshot.id}
									className="flex flex-wrap items-baseline gap-x-4"
								>
									<span className="min-w-0 font-semibold wrap-anywhere">
										{host.hostname}
									</span>
									<time dateTime={new Date(snapshot.time).toISOString()}>
										{formatDateTime(snapshot.time)}
									</time>
									<span className="font-mono">{snapshot.id.slice(0, 8)}</span>
								</li>
							))}
						</ul>
						{more > 0 && (
							<p className="text-muted-foreground">{text.more(more)}</p>
						)}
					</div>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel>{text.keep}</AlertDialogCancel>
					<AlertDialogAction variant="destructive" onClick={onConfirm}>
						{text.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
