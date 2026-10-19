import {
	Ban,
	Circle,
	CircleCheck,
	CircleDashed,
	CircleSlash,
	CircleX,
	Clock,
	Hourglass,
	Loader2,
	Minus,
	TriangleAlert,
	Unplug,
} from "lucide-react";
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
	queued: { icon: Clock, tone: "text-neutral", label: copy.jobs.status.queued },
	running: {
		icon: Loader2,
		tone: "text-foreground motion-safe:[&>svg]:animate-spin",
		label: copy.jobs.status.running,
	},
	waiting: {
		icon: Hourglass,
		tone: "text-neutral",
		label: copy.jobs.status.waiting,
	},
	succeeded: {
		icon: CircleCheck,
		tone: "text-success",
		label: copy.jobs.status.succeeded,
	},
	warnings: {
		icon: TriangleAlert,
		tone: "text-warning",
		label: copy.jobs.status.warnings,
	},
	failed: {
		icon: CircleX,
		tone: "text-destructive",
		label: copy.jobs.status.failed,
	},
	cancelled: {
		icon: CircleSlash,
		tone: "text-neutral",
		label: copy.jobs.status.cancelled,
	},
	interrupted: {
		icon: Unplug,
		tone: "text-warning",
		label: copy.jobs.status.interrupted,
	},
	not_started: {
		icon: CircleDashed,
		tone: "text-neutral",
		label: copy.jobs.status.not_started,
	},
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
