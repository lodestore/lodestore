import { MoreHorizontal } from "lucide-react";
import type { SnapshotItem } from "@/api/gen/model";
import type { SnapshotAction } from "@/components/snapshots/snapshot-dialogs";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { copy } from "@/copy";
import { shortId } from "@/lib/snapshot";

const text = copy.snapshots.menu;

export function SnapshotActionsMenu({
	snapshot,
	onAction,
}: {
	snapshot: SnapshotItem;
	onAction: (action: SnapshotAction) => void;
}) {
	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					data-snapshot-actions={snapshot.id}
					aria-label={text.actionsFor(shortId(snapshot.id))}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem
					onSelect={() => onAction(snapshot.suspicious ? "clear" : "mark")}
				>
					{snapshot.suspicious ? text.clear : text.mark}
				</DropdownMenuItem>
				<DropdownMenuSeparator />
				<DropdownMenuItem
					variant="destructive"
					onSelect={() => onAction("delete")}
				>
					{text.delete}
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}
