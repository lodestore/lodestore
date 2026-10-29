import { CircleCheck, CircleDashed, CircleX, Clock } from "lucide-react";
import type { ComponentProps, ComponentType } from "react";
import type { HostState } from "@/api/gen/model";
import { type Status, StatusBadge } from "@/components/status-badge";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const text = copy.overview.states;

function OfflineDot({ className, ...props }: ComponentProps<"svg">) {
	return (
		<svg
			viewBox="0 0 8 8"
			className={cn(className, "m-1 size-2! fill-current")}
			{...props}
		>
			<circle cx="4" cy="4" r="4" />
		</svg>
	);
}

export const HOST_STATES: {
	state: HostState;
	label: string;
	icon: ComponentType<ComponentProps<"svg">>;
	tone: string;
	segment: string;
}[] = [
	{
		state: "failed",
		label: text.failed,
		icon: CircleX,
		tone: "text-destructive",
		segment: "bg-destructive",
	},
	{
		state: "overdue",
		label: text.overdue,
		icon: Clock,
		tone: "text-warning",
		segment: "bg-warning",
	},
	{
		state: "offline",
		label: text.offline,
		icon: OfflineDot,
		tone: "text-neutral",
		segment: "bg-neutral",
	},
	{
		state: "not_protected",
		label: text.notProtected,
		icon: CircleDashed,
		tone: "text-neutral",
		segment: "border border-neutral",
	},
	{
		state: "healthy",
		label: text.healthy,
		icon: CircleCheck,
		tone: "text-muted-foreground",
		segment: "bg-success",
	},
];

const badgeStatus: Record<HostState, Status> = {
	failed: "failed",
	overdue: "overdue",
	offline: "offline",
	not_protected: "not_protected",
	healthy: "protected",
};

export function HostStateBadge({ state }: { state: HostState }) {
	return <StatusBadge status={badgeStatus[state]} />;
}
