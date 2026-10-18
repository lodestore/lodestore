import { MoreHorizontal } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

export function EditorFooter({
	dirty,
	canSave,
	canDiscard,
	saving,
	onSave,
	onDiscard,
}: {
	dirty: boolean;
	canSave: boolean;
	canDiscard: boolean;
	saving: boolean;
	onSave: () => void;
	onDiscard: () => void;
}) {
	const text = copy.policies;
	return (
		<div className="sticky bottom-0 z-10 flex h-16 items-center justify-between gap-4 border-t bg-background">
			<div className="flex items-center gap-2">
				<Button
					type="button"
					variant={dirty ? "default" : "outline"}
					disabled={!canSave}
					onClick={onSave}
				>
					{saving && <Spinner />}
					{text.save}
				</Button>
				{canDiscard && (
					<>
						<Button
							type="button"
							variant="outline"
							disabled={saving}
							className="hidden sm:inline-flex"
							onClick={onDiscard}
						>
							{text.discard}
						</Button>
						<DropdownMenu modal={false}>
							<DropdownMenuTrigger asChild>
								<Button
									type="button"
									variant="ghost"
									size="icon"
									className="sm:hidden"
									aria-label={text.footer.more}
								>
									<MoreHorizontal aria-hidden />
								</Button>
							</DropdownMenuTrigger>
							<DropdownMenuContent align="start">
								<DropdownMenuItem disabled={saving} onSelect={onDiscard}>
									{text.discard}
								</DropdownMenuItem>
							</DropdownMenuContent>
						</DropdownMenu>
					</>
				)}
			</div>
			<p className="hidden items-center gap-2 text-sm text-muted-foreground sm:flex">
				<span
					aria-hidden
					className={cn(
						"size-2 rounded-full",
						dirty ? "bg-warning" : "bg-success",
					)}
				/>
				{dirty ? text.footer.unsaved : text.footer.saved}
			</p>
		</div>
	);
}
