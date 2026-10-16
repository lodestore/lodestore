import type { Selection } from "@/components/restore/use-selection";
import { Button } from "@/components/ui/button";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";

export function SelectionBar({
	selection,
	actions,
	hint = true,
}: {
	selection: Selection;
	actions?: React.ReactNode;
	/** False when there is nothing to select, so the hint would only add noise. */
	hint?: boolean;
}) {
	const text = copy.restore.selection;
	if (selection.count === 0 && !hint) return null;
	return (
		<div className="sticky bottom-0 z-10 flex h-16 items-center justify-between gap-4 border-t bg-card px-4">
			{selection.count > 0 ? (
				<>
					<div className="flex min-w-0 items-center gap-4">
						<div className="min-w-0">
							<p aria-hidden className="font-semibold">
								{text.selected(selection.count)}
							</p>
							<p className="truncate text-sm text-muted-foreground">
								{text.folders}
							</p>
						</div>
						<Button
							type="button"
							variant="outline"
							size="sm"
							onClick={() => selection.clear()}
						>
							{text.clear}
						</Button>
					</div>
					<div className="flex items-center gap-2">{actions}</div>
				</>
			) : (
				<p className="text-sm text-muted-foreground">{text.hint}</p>
			)}
		</div>
	);
}

export function SelectionActions({
	ready,
	canRestore,
	onRestore,
	restoreRef,
	onDownload,
	downloadRef,
}: {
	/** False until the snapshot details the dialogs need have loaded. */
	ready: boolean;
	canRestore: boolean;
	onRestore: () => void;
	restoreRef: React.Ref<HTMLButtonElement>;
	onDownload?: () => void;
	downloadRef?: React.Ref<HTMLButtonElement>;
}) {
	const text = copy.restore.actions;
	const restore = (
		<Button
			ref={restoreRef}
			type="button"
			disabled={!ready || !canRestore}
			onClick={onRestore}
		>
			{text.restore}
		</Button>
	);
	return (
		<>
			{onDownload && (
				<Button
					ref={downloadRef}
					type="button"
					variant="outline"
					disabled={!ready}
					onClick={onDownload}
				>
					{text.download}
				</Button>
			)}
			{canRestore || !ready ? (
				restore
			) : (
				<Tooltip>
					<TooltipTrigger asChild>
						<span tabIndex={0}>{restore}</span>
					</TooltipTrigger>
					<TooltipContent>{text.noTarget}</TooltipContent>
				</Tooltip>
			)}
		</>
	);
}
