import { CircleAlert, Info, TriangleAlert } from "lucide-react";
import { CapabilityChecklist } from "@/components/capability-checklist";
import {
	GuaranteeNotes,
	levelName,
	UNCHECKED_LEVEL,
} from "@/components/guarantee-notes";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldLabel } from "@/components/ui/field";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";
import {
	backendOf,
	type CheckState,
	checkRows,
	type Draft,
	needsConsent,
	runningRows,
} from "./draft";

export function ProbeResultPanel({
	draft,
	check,
	onAccepted,
	onCheckAgain,
	className,
}: {
	draft: Draft;
	check: CheckState;
	onAccepted: (accepted: boolean) => void;
	onCheckAgain: () => void;
	className?: string;
}) {
	const running = check.phase === "running";
	return (
		<Card className={cn("gap-4", className)}>
			<CardContent className="flex flex-col gap-4 wrap-anywhere">
				<h3 className="font-semibold">{copy.rights.result}</h3>
				{check.phase === "idle" && (
					<p className="text-sm text-muted-foreground">
						{copy.rights.resultIdle}
					</p>
				)}
				{check.phase === "running" && (
					<CapabilityChecklist rows={runningRows()} />
				)}
				{check.phase === "error" && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertDescription>{check.message}</AlertDescription>
					</Alert>
				)}
				{check.phase === "done" && (
					<DoneResult
						draft={draft}
						check={check}
						running={running}
						onAccepted={onAccepted}
						onCheckAgain={onCheckAgain}
					/>
				)}
			</CardContent>
		</Card>
	);
}

function DoneResult({
	draft,
	check,
	running,
	onAccepted,
	onCheckAgain,
}: {
	draft: Draft;
	check: Extract<CheckState, { phase: "done" }>;
	running: boolean;
	onAccepted: (accepted: boolean) => void;
	onCheckAgain: () => void;
}) {
	const { result, stale } = check;
	const level = result.guarantee_level;
	const consent = !stale && needsConsent(result);
	const unchecked = level === UNCHECKED_LEVEL;
	return (
		<>
			{result.existing_repository && (
				<Alert>
					<Info aria-hidden />
					<AlertTitle>{copy.existingRepo.heading}</AlertTitle>
					<AlertDescription>{copy.existingRepo.body}</AlertDescription>
				</Alert>
			)}
			{!stale && result.blocking && (
				<Alert variant="destructive" role="alert">
					<CircleAlert aria-hidden />
					<AlertDescription id="continue-blocked">
						{copy.check.blockedContinue}
					</AlertDescription>
				</Alert>
			)}
			<CapabilityChecklist
				rows={checkRows(result, backendOf(draft.preset) ?? "s3", stale)}
				stale={stale}
			/>
			{!stale && !consent && (
				<div className="flex flex-col gap-1 text-sm">
					<p>{copy.guarantee.levelLine(levelName(level))}</p>
					<GuaranteeNotes codes={result.guarantee_notes} />
				</div>
			)}
			{consent && (
				<Alert className="text-warning">
					<TriangleAlert aria-hidden />
					<AlertTitle>
						{unchecked
							? copy.guarantee.uncheckedTitle
							: copy.guarantee.weakTitle}
					</AlertTitle>
					<AlertDescription className="flex flex-col gap-4 [&_p:not(:last-child)]:mb-0">
						{unchecked && <p>{copy.guarantee.uncheckedBody}</p>}
						<GuaranteeNotes codes={result.guarantee_notes} />
						{unchecked && (
							<div>
								<Button
									type="button"
									variant="outline"
									className="text-foreground"
									disabled={running}
									onClick={onCheckAgain}
								>
									{copy.guarantee.checkAgain}
								</Button>
							</div>
						)}
						<Field orientation="horizontal">
							<Checkbox
								id="accept-weaker-guarantee"
								checked={draft.acceptWeakerGuarantee}
								onCheckedChange={(next) => onAccepted(next === true)}
							/>
							<FieldLabel htmlFor="accept-weaker-guarantee">
								{copy.guarantee.consentLabel}
							</FieldLabel>
						</Field>
					</AlertDescription>
				</Alert>
			)}
		</>
	);
}
