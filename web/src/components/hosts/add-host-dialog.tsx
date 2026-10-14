import { useState } from "react";
import { AddHostPanel } from "@/components/add-host-panel";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { copy } from "@/copy";

export function AddHostDialog({
	open,
	onOpenChange,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
}) {
	const [connected, setConnected] = useState(false);

	return (
		<Dialog
			open={open}
			onOpenChange={(next) => {
				if (!next) setConnected(false);
				onOpenChange(next);
			}}
		>
			<DialogContent
				className="max-h-[90svh] overflow-y-auto sm:max-w-[560px]"
				aria-describedby={undefined}
			>
				<DialogHeader>
					<DialogTitle>{copy.addHost.title}</DialogTitle>
				</DialogHeader>
				<AddHostPanel onConnected={() => setConnected(true)} />
				{connected && (
					<DialogFooter>
						<Button onClick={() => onOpenChange(false)}>
							{copy.addHost.finish}
						</Button>
					</DialogFooter>
				)}
			</DialogContent>
		</Dialog>
	);
}
