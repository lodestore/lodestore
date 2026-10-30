import { CircleArrowUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

const text = copy.agentUpdate;

export function UpdateSelectionBar({
	count,
	onUpdate,
	onClear,
}: {
	count: number;
	onUpdate: () => void;
	onClear: () => void;
}) {
	if (count === 0) return null;
	return (
		<div className="flex flex-wrap items-center gap-2 rounded-xl bg-primary-subtle px-4 py-2">
			<span className="font-semibold">{text.selected(count)}</span>
			<Button variant="outline" size="sm" onClick={onUpdate}>
				<CircleArrowUp aria-hidden />
				{text.updateSelected}
			</Button>
			<Button variant="outline" size="sm" onClick={onClear}>
				{text.clear}
			</Button>
		</div>
	);
}
