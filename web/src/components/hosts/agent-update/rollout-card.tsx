import { Link } from "@tanstack/react-router";
import {
	CircleCheck,
	CircleX,
	LoaderCircle,
	type LucideIcon,
	Pause,
	Play,
} from "lucide-react";
import { useId, useRef, useState } from "react";
import type { Rollout } from "@/api/gen/model";
import { RolloutSteps } from "@/components/hosts/agent-update/rollout-steps";
import { TextWithPaths } from "@/components/path-text";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const text = copy.agentUpdate;
const ROLLBACKS_SHOWN = 5;

function pausedLine(rollout: Rollout) {
	if (rollout.pause_reason)
		return text.card.pausedBecause(rollout.pause_reason);
	return rollout.rollbacks_since_resume > rollout.pause_after
		? text.card.paused(rollout.rollbacks_since_resume, rollout.pause_after)
		: text.card.pausedManual;
}

function status(rollout: Rollout): {
	line: string;
	icon: LucideIcon;
	tone: string;
	spin?: boolean;
} {
	const done = rollout.updated + rollout.rolled_back;
	switch (rollout.state) {
		case "running":
			return {
				line: text.card.running(done, rollout.total),
				icon: LoaderCircle,
				tone: "text-primary-strong",
				spin: true,
			};
		case "paused":
			return {
				line: pausedLine(rollout),
				icon: Pause,
				tone: "text-warning",
			};
		case "done":
			return rollout.rolled_back > 0
				? {
						line: text.card.doneRollbacks(rollout.updated, rollout.rolled_back),
						icon: CircleCheck,
						tone: "text-warning",
					}
				: {
						line: text.card.done(rollout.updated),
						icon: CircleCheck,
						tone: "text-success",
					};
		case "cancelled":
			return {
				line: text.card.cancelled(rollout.updated),
				icon: CircleX,
				tone: "text-neutral",
			};
	}
}

export function RolloutCard({
	rollout,
	pending,
	onPause,
	onResume,
	onCancel,
	onDismiss,
}: {
	rollout: Rollout;
	pending: boolean;
	onPause: (id: string) => void;
	onResume: (id: string) => void;
	onCancel: (id: string) => void;
	onDismiss: (id: string) => void;
}) {
	const titleId = useId();
	const [cancelling, setCancelling] = useState(false);
	const cancelButton = useRef<HTMLButtonElement>(null);
	const s = status(rollout);
	const Icon = s.icon;
	const [head, ...tail] = s.line.split(" · ");
	const rollbacks = rollout.rollbacks.slice(0, ROLLBACKS_SHOWN);
	const finished = rollout.state === "done" || rollout.state === "cancelled";
	return (
		<section aria-labelledby={titleId}>
			<Card className="px-6">
				<div className="flex flex-wrap items-start justify-between gap-4">
					<div className="flex flex-col gap-1">
						<h2 id={titleId} className="text-base font-semibold">
							{text.card.title}{" "}
							<span className="font-mono">{rollout.target_version}</span>
						</h2>
						<p
							aria-live="polite"
							className={cn("flex items-center gap-2 text-sm", s.tone)}
						>
							<Icon
								aria-hidden
								className={cn(
									"size-4 shrink-0",
									s.spin && "animate-spin motion-reduce:animate-none",
								)}
							/>
							<span>
								<strong className="font-semibold">{head}</strong>
								{tail.length > 0 && (
									<span className="text-muted-foreground">
										{" · "}
										{tail.join(" · ")}
									</span>
								)}
							</span>
						</p>
					</div>
					<div className="flex flex-wrap gap-2">
						{rollout.state === "running" && (
							<Button
								variant="outline"
								disabled={pending}
								onClick={() => onPause(rollout.id)}
							>
								<Pause aria-hidden />
								{text.pause}
							</Button>
						)}
						{rollout.state === "paused" && (
							<Button
								variant="outline"
								disabled={pending}
								onClick={() => onResume(rollout.id)}
							>
								<Play aria-hidden />
								{text.resume}
							</Button>
						)}
						{!finished && (
							<Button
								ref={cancelButton}
								variant="outline"
								className="text-destructive"
								disabled={pending}
								onClick={() => setCancelling(true)}
							>
								{text.cancel}
							</Button>
						)}
						{finished && (
							<Button
								variant="outline"
								disabled={pending}
								onClick={() => onDismiss(rollout.id)}
							>
								{text.dismiss}
							</Button>
						)}
					</div>
				</div>
				<RolloutSteps waves={rollout.waves} state={rollout.state} />
				{rollbacks.length > 0 && (
					<ul className="flex flex-col gap-2 text-sm">
						{rollbacks.map((r) => (
							<li
								key={r.update_id}
								className="flex flex-col gap-1 md:flex-row md:gap-2"
							>
								<span className="font-semibold break-words">{r.hostname}</span>
								{r.reason && (
									<span className="min-w-0 text-muted-foreground">
										<TextWithPaths text={r.reason} />
									</span>
								)}
							</li>
						))}
					</ul>
				)}
				<Link
					to="/jobs"
					search={{ kind: "agent_update" }}
					className="self-start rounded-sm text-sm text-primary-strong underline underline-offset-4"
				>
					{text.jobs}
				</Link>
			</Card>
			<AlertDialog open={cancelling} onOpenChange={setCancelling}>
				<AlertDialogContent
					onCloseAutoFocus={(event) => {
						event.preventDefault();
						cancelButton.current?.focus();
					}}
				>
					<AlertDialogHeader>
						<AlertDialogTitle>{text.cancelTitle}</AlertDialogTitle>
						<AlertDialogDescription>
							{text.cancelConfirm(rollout.target_version)}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel>{text.keep}</AlertDialogCancel>
						<AlertDialogAction
							variant="destructive"
							onClick={() => onCancel(rollout.id)}
						>
							{text.cancel}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</section>
	);
}
