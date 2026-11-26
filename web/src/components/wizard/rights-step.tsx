import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { AgentRightsTasks, Task } from "./agent-rights-tasks";
import {
	agentCanSkip,
	azureVendsSas,
	type CheckState,
	type Draft,
	needsConsent,
	type PresetInfo,
	rightsReady,
} from "./draft";
import { StepCard, StepHeading } from "./parts";
import { ProbeResultPanel } from "./probe-result-panel";

export function RightsStep({
	draft,
	preset,
	onChange,
	check,
	onCheck,
	onSkip,
	onBack,
	onContinue,
}: {
	draft: Draft;
	preset: PresetInfo;
	onChange: (patch: Partial<Draft>) => void;
	check: CheckState;
	onCheck: () => void;
	onSkip: () => void;
	onBack: () => void;
	onContinue: () => void;
}) {
	const running = check.phase === "running";
	const fresh = check.phase === "done" && !check.stale;
	const blocked = fresh && check.result.blocking;
	const needsAccept =
		fresh && needsConsent(check.result) && !draft.acceptWeakerGuarantee;
	const ready = rightsReady(draft);

	return (
		<StepCard
			onBack={onBack}
			backDisabled={running}
			actions={
				<>
					{agentCanSkip(draft) && (
						<Button
							type="button"
							variant="outline"
							disabled={running}
							onClick={
								draft.skipRights
									? () => onChange({ skipRights: false })
									: onSkip
							}
						>
							{draft.skipRights ? copy.rights.useLimited : copy.rights.skip}
						</Button>
					)}
					<Button
						type="button"
						disabled={!fresh || blocked || needsAccept}
						aria-describedby={blocked ? "continue-blocked" : undefined}
						onClick={onContinue}
					>
						{copy.wizard.continueToKey}
					</Button>
				</>
			}
		>
			<StepHeading>{copy.rights.title}</StepHeading>
			<p>{copy.rights.lead}</p>
			<div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_320px] lg:items-start">
				<div className="flex min-w-0 flex-col gap-6">
					<AgentRightsTasks
						preset={preset}
						bucket={draft.bucket.trim()}
						prefix={draft.prefix.trim()}
						vendsSas={azureVendsSas(draft)}
						agent={draft.agent}
						onAgent={(agent) => onChange({ agent, skipRights: false })}
						disabled={running}
						conditionalPassed={
							check.phase === "done" &&
							check.result.checks.some(
								(c) => c.id === "conditional_write" && c.status === "pass",
							)
						}
					/>
					<Task
						number={3}
						title={copy.rights.check}
						done={fresh && !draft.skipRights}
					>
						<p className="text-sm text-muted-foreground">
							{copy.rights.checkBody}
						</p>
						<div>
							<Button
								type="button"
								variant={check.phase === "idle" ? "default" : "outline"}
								disabled={running || !ready}
								onClick={onCheck}
							>
								{running && <Spinner />}
								{check.phase === "idle" || check.phase === "error"
									? copy.rights.checkButton
									: copy.rights.checkAgain}
							</Button>
						</div>
					</Task>
				</div>
				<ProbeResultPanel
					className="lg:sticky lg:top-6"
					draft={draft}
					check={check}
					onAccepted={(acceptWeakerGuarantee) =>
						onChange({ acceptWeakerGuarantee })
					}
					onCheckAgain={onCheck}
				/>
			</div>
		</StepCard>
	);
}
