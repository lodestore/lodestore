import { StatusBadge } from "@/components/status-badge";
import { copy } from "@/copy";

export function AgentKeyBadge({ state }: { state: string }) {
	if (state === "verified") {
		return (
			<StatusBadge status="success">{copy.repos.agentKeyVerified}</StatusBadge>
		);
	}
	if (state === "can_delete") {
		return (
			<StatusBadge status="warning">{copy.repos.agentKeyCanDelete}</StatusBadge>
		);
	}
	if (state === "can_overwrite") {
		return (
			<StatusBadge status="warning">
				{copy.repos.agentKeyCanOverwrite}
			</StatusBadge>
		);
	}
	return (
		<StatusBadge status="neutral">{copy.repos.agentKeyNotChecked}</StatusBadge>
	);
}
