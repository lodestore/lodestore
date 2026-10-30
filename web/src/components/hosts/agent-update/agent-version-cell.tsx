import { Link } from "@tanstack/react-router";
import { CircleArrowUp, CircleX, Clock, LoaderCircle } from "lucide-react";
import type { AgentUpdateStatus, HostUpdate } from "@/api/gen/model";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const text = copy.agentUpdate;

export function blockerReason(
	update: HostUpdate,
	status?: AgentUpdateStatus,
): string | undefined {
	switch (update.blocker) {
		case "no_self_update":
			return text.manual;
		case "unsigned":
			return (
				status &&
				`${text.unsigned.head} ${text.unsigned.body(status.agent_dir)}`
			);
		case "no_binary":
			return (
				status &&
				`${text.noBinary.head(status.target_version ?? "", update.arch)} ${text.noBinary.body(update.arch, status.agent_dir)}`
			);
		default:
			return undefined;
	}
}

function Line({
	tone,
	icon: Icon,
	title,
	spin,
	children,
}: {
	tone: string;
	icon: typeof Clock;
	title?: string;
	spin?: boolean;
	children: React.ReactNode;
}) {
	return (
		<span
			title={title}
			className={cn("flex items-start gap-1 text-sm whitespace-normal", tone)}
		>
			<span className="flex h-5 shrink-0 items-center">
				<Icon
					aria-hidden
					className={cn(
						"size-4",
						spin && "animate-spin motion-reduce:animate-none",
					)}
				/>
			</span>
			{children}
		</span>
	);
}

function StateLine({
	update,
	status,
}: {
	update: HostUpdate;
	status?: AgentUpdateStatus;
}) {
	switch (update.state) {
		case "queued":
		case "waiting_for_job":
		case "offline_queued":
			return (
				<Line tone="text-neutral" icon={Clock}>
					{text.state[update.state]}
				</Line>
			);
		case "updating":
			return (
				<Line tone="text-primary-strong" icon={LoaderCircle} spin>
					{text.state.updating(update.target_version ?? "")}
				</Line>
			);
		case "rolled_back":
			return (
				<Line
					tone="text-destructive"
					icon={CircleX}
					title={update.reason ?? undefined}
				>
					{update.update_id ? (
						<Link
							to="/jobs/$id"
							params={{ id: update.update_id }}
							className="rounded-sm underline-offset-4 hover:underline"
						>
							{text.state.rolled_back}
						</Link>
					) : (
						text.state.rolled_back
					)}
				</Line>
			);
		default:
			if (!update.outdated) return null;
			return (
				<Line
					tone="text-warning"
					icon={CircleArrowUp}
					title={blockerReason(update, status)}
				>
					{text.outdated}
				</Line>
			);
	}
}

export function AgentVersionCell({
	version,
	update,
	status,
}: {
	version: string;
	update?: HostUpdate;
	status?: AgentUpdateStatus;
}) {
	return (
		<div className="flex flex-col gap-1">
			<span className="font-mono">{update?.version ?? version}</span>
			{update && <StateLine update={update} status={status} />}
		</div>
	);
}
