import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { AddHostPanel } from "@/components/add-host-panel";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";
import { StepCard, StepHeading } from "./parts";
import { text } from "./text";

export function FirstHostStep({ repositoryId }: { repositoryId: string }) {
	const [connected, setConnected] = useState(false);

	return (
		<StepCard
			actions={
				connected ? (
					<Button asChild>
						<Link to="/hosts">{copy.wizard.openHosts}</Link>
					</Button>
				) : (
					<Button variant="ghost" asChild>
						<Link to="/hosts">{copy.wizard.skipHost}</Link>
					</Button>
				)
			}
		>
			<StepHeading>{text.stepHeading.host}</StepHeading>
			<AddHostPanel
				repositoryId={repositoryId}
				onConnected={() => setConnected(true)}
			/>
		</StepCard>
	);
}
