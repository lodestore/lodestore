import { Link } from "@tanstack/react-router";
import { Circle, CircleCheck } from "lucide-react";
import { useState } from "react";
import type { TokenHost } from "@/api/gen/model";
import { AddHostPanel, type AddHostStage } from "@/components/add-host-panel";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";
import { StepCard, StepHeading } from "./parts";
import { SuccessMoment } from "./success-moment";
import { text } from "./text";

const ACTIVE_ITEM: Record<AddHostStage, number> = {
	idle: -1,
	issued: 0,
	enrolling: 1,
	online: 3,
};

export function FirstHostStep({
	repositoryId,
	rebuilding = false,
}: {
	repositoryId: string;
	rebuilding?: boolean;
}) {
	const [connected, setConnected] = useState<TokenHost>();
	const [stage, setStage] = useState<AddHostStage>("idle");
	const active = ACTIVE_ITEM[stage];

	return (
		<StepCard
			actions={
				connected ? (
					<Button asChild>
						<Link to="/hosts">{copy.wizard.openHosts}</Link>
					</Button>
				) : (
					<Button variant="outline" asChild>
						<Link to="/hosts">{copy.wizard.skipHost}</Link>
					</Button>
				)
			}
		>
			<StepHeading>{text.stepHeading.host}</StepHeading>
			{rebuilding && (
				<p className="text-sm text-muted-foreground">
					{copy.catalog.connectedExisting}{" "}
					<Link to="/restore" className="text-primary-strong underline">
						{copy.catalog.openRestore}
					</Link>
				</p>
			)}
			<div className="grid gap-6 xl:grid-cols-2">
				<AddHostPanel
					repositoryId={repositoryId}
					onStage={setStage}
					onConnected={setConnected}
				/>
				<Card className="order-first gap-6 px-8 [--card-spacing:--spacing(8)] xl:order-none">
					{connected ? (
						<SuccessMoment
							hostname={connected.hostname}
							version={connected.version}
						/>
					) : (
						<>
							<h3 className="text-base font-semibold">
								{copy.firstHost.whatNext}
							</h3>
							<ol className="flex flex-col gap-4">
								{copy.firstHost.timeline.map((label, index) => {
									const done = index < active;
									const current = index === active;
									return (
										<li
											key={label}
											aria-current={current ? "step" : undefined}
											data-state={
												done ? "done" : current ? "active" : "pending"
											}
											className={cn(
												"flex items-center gap-2 text-sm",
												current
													? "text-primary-strong"
													: !done && "text-muted-foreground",
											)}
										>
											{done ? (
												<CircleCheck
													aria-hidden
													className="size-5 text-success"
												/>
											) : current ? (
												<Spinner className="size-5" />
											) : (
												<Circle aria-hidden className="size-5" />
											)}
											{label}
										</li>
									);
								})}
							</ol>
						</>
					)}
				</Card>
			</div>
		</StepCard>
	);
}
