import { CircleArrowUp, Info } from "lucide-react";
import type { AgentUpdateStatus } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

const text = copy.agentUpdate;

export function OutdatedBanner({
	status,
	count,
	differs,
	onUpdate,
}: {
	status?: AgentUpdateStatus;
	count: number;
	differs: boolean;
	onUpdate: () => void;
}) {
	if (!status || status.rollout) return null;
	if (!status.signed) {
		if (!differs) return null;
		return (
			<Alert role="status">
				<Info aria-hidden />
				<AlertDescription>
					<strong className="font-semibold text-foreground">
						{text.unsigned.head}
					</strong>{" "}
					{text.unsigned.body(status.agent_dir)}
				</AlertDescription>
			</Alert>
		);
	}
	if (count === 0) return null;
	return (
		<Alert role="status" className="border-warning">
			<CircleArrowUp aria-hidden className="text-warning" />
			<AlertDescription>
				<strong className="font-semibold text-foreground">
					{text.banner.head(count)}
				</strong>{" "}
				{text.banner.body(status.target_version ?? "")}
			</AlertDescription>
			<div className="col-start-2 mt-2">
				<Button variant="outline" onClick={onUpdate}>
					<CircleArrowUp aria-hidden />
					{text.updateOutdated}
				</Button>
			</div>
		</Alert>
	);
}
