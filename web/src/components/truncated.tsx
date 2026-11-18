import { TextWithPaths } from "@/components/path-text";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export function Truncated({
	text,
	className,
}: {
	text: string;
	className?: string;
}) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span className={cn("block truncate", className)}>{text}</span>
			</TooltipTrigger>
			<TooltipContent className="max-w-lg wrap-anywhere">
				<TextWithPaths text={text} />
			</TooltipContent>
		</Tooltip>
	);
}
