import { ArrowDown, ArrowUp, ChevronsUpDown } from "lucide-react";
import { cn } from "@/lib/utils";

export function SortButton({
	label,
	active,
	dir,
	onClick,
	className,
}: {
	label: string;
	active: boolean;
	dir: "asc" | "desc";
	onClick: () => void;
	className?: string;
}) {
	const Icon = !active ? ChevronsUpDown : dir === "asc" ? ArrowUp : ArrowDown;
	return (
		<button
			type="button"
			onClick={onClick}
			className={cn(
				"inline-flex min-h-6 items-center gap-1 rounded-sm hover:text-foreground",
				className,
			)}
		>
			{label}
			<Icon aria-hidden className="size-3.5" />
		</button>
	);
}
