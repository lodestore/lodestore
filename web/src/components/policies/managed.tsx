import { GitBranch, TriangleAlert } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";

export function ManagedAlert({ manager }: { manager: string }) {
	return (
		<Alert className="border-warning/60">
			<TriangleAlert aria-hidden className="text-warning" />
			<AlertDescription className="text-foreground wrap-anywhere">
				{copy.policies.managed.alert(manager)}
			</AlertDescription>
		</Alert>
	);
}

export function ManagedBadge({ manager }: { manager: string }) {
	const hint = copy.policies.managed.hint(manager);
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<Badge
					variant="outline"
					role="img"
					tabIndex={0}
					aria-label={hint}
					className="shrink-0"
				>
					<GitBranch aria-hidden />
					{copy.policies.managed.badge}
				</Badge>
			</TooltipTrigger>
			<TooltipContent className="wrap-anywhere">{hint}</TooltipContent>
		</Tooltip>
	);
}
