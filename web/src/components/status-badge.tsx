import { Ban, Circle, CircleCheck, Minus, TriangleAlert } from "lucide-react";
import type { ReactNode } from "react";
import { Badge } from "@/components/ui/badge";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const statuses = {
	online: {
		icon: Circle,
		tone: "text-success",
		label: copy.hosts.status.online,
	},
	offline: {
		icon: Circle,
		tone: "text-neutral",
		label: copy.hosts.status.offline,
	},
	revoked: {
		icon: Ban,
		tone: "text-destructive",
		label: copy.hosts.status.revoked,
	},
	success: { icon: CircleCheck, tone: "text-success", label: "" },
	warning: { icon: TriangleAlert, tone: "text-warning", label: "" },
	neutral: { icon: Minus, tone: "text-neutral", label: "" },
} as const;

export type Status = keyof typeof statuses;

export function StatusBadge({
	status,
	children,
	className,
}: {
	status: Status;
	children?: ReactNode;
	className?: string;
}) {
	const { icon: Icon, tone, label } = statuses[status];
	const dot = Icon === Circle;
	return (
		<Badge
			variant="outline"
			data-status={status}
			className={cn("gap-1", tone, className)}
		>
			<Icon aria-hidden className={cn(dot && "size-2! fill-current")} />
			<span>{children ?? label}</span>
		</Badge>
	);
}
