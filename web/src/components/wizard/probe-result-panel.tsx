import { CircleAlert, CircleCheck, Info, TriangleAlert } from "lucide-react";
import type { CheckResult } from "@/api/gen/model";
import {
	CapabilityChecklist,
	type CheckId,
} from "@/components/capability-checklist";
import { LevelLine, LevelScale } from "@/components/guarantee-level";
import { GuaranteeNotes, UNCHECKED_LEVEL } from "@/components/guarantee-notes";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Field, FieldLabel } from "@/components/ui/field";
import {
	Item,
	ItemActions,
	ItemContent,
	ItemDescription,
	ItemGroup,
	ItemMedia,
	ItemTitle,
} from "@/components/ui/item";
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

const BUCKET_BACKENDS = ["s3", "gcs", "azblob"];
const MAIN_IDS: readonly CheckId[] = [
	"write",
	"list",
	"conditional_write",
	"agent_delete",
];
const LOCK_IDS: readonly CheckId[] = ["object_lock"];
const notes = copy.guarantee.notes as Record<string, string>;

export type BucketFact = {
	id: "versioning" | "lifecycle";
	status: "pass" | "warn";
	label: string;
	detail: string;
	codes: string[];
};

export function bucketFacts(
	result: CheckResult,
	backend: string,
): BucketFact[] {
	if (!BUCKET_BACKENDS.includes(backend)) return [];
	const facts = copy.rights.facts;
	const has = (code: string) => result.guarantee_notes.includes(code);
	const level = result.guarantee_level;
	const out: BucketFact[] = [];
	if (level === "overwrite_keeps_old_version") {
		out.push({
			id: "versioning",
			status: "pass",
			label: facts.versioning,
			detail: facts.versioningOn,
			codes: [],
		});
	} else if (has("agent_can_overwrite")) {
		out.push({
			id: "versioning",
			status: "warn",
			label: facts.versioning,
			detail: facts.versioningOff,
			codes: ["agent_can_overwrite"],
		});
	}
	const lifecycle = ["lifecycle_deletes", "lifecycle_unverified"].filter(has);
	if (lifecycle.length > 0) {
		out.push({
			id: "lifecycle",
			status: "warn",
			label: facts.lifecycle,
			detail: lifecycle.map((code) => notes[code]).join(" "),
			codes: lifecycle,
		});
	} else if (level !== UNCHECKED_LEVEL) {
		out.push({
			id: "lifecycle",
			status: "pass",
			label: facts.lifecycle,
			detail: facts.lifecycleOk,
			codes: [],
		});
	}
	return out;
}

function FactRow({ fact }: { fact: BucketFact }) {
	const pass = fact.status === "pass";
	const Icon = pass ? CircleCheck : TriangleAlert;
	return (
		<Item
			role="listitem"
			variant="outline"
			size="sm"
			data-status={fact.status}
			data-fact={fact.id}
		>
			<ItemMedia variant="icon" className="size-6 border-0 bg-transparent">
				<Icon
					aria-hidden
					className={cn("size-6", pass ? "text-success" : "text-warning")}
				/>
			</ItemMedia>
			<ItemContent>
				<ItemTitle>{fact.label}</ItemTitle>
				<ItemDescription className="line-clamp-none">
					{fact.detail}
				</ItemDescription>
			</ItemContent>
			<ItemActions
				className={cn("text-xs", pass ? "text-success" : "text-warning")}
			>
				{copy.check.status[fact.status]}
			</ItemActions>
		</Item>
	);
}

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
	const backend = backendOf(draft.preset) ?? "s3";
	const grouped = BUCKET_BACKENDS.includes(backend);
	const facts = stale ? [] : bucketFacts(result, backend);
	const shown = new Set(facts.flatMap((fact) => fact.codes));
	const remaining = result.guarantee_notes.filter((code) => !shown.has(code));
	const rows = checkRows(result, backend, stale);
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
				rows={rows}
				stale={stale}
				ids={grouped ? MAIN_IDS : undefined}
			/>
			{grouped && (
				<div className="flex flex-col gap-2">
					<h4 className="text-sm font-semibold">{copy.rights.bucketGroup}</h4>
					<CapabilityChecklist
						rows={rows}
						stale={stale}
						ids={LOCK_IDS}
						hideSummary
					/>
					{facts.length > 0 && (
						<ItemGroup className="gap-2">
							{facts.map((fact) => (
								<FactRow key={fact.id} fact={fact} />
							))}
						</ItemGroup>
					)}
				</div>
			)}
			{!stale && (
				<div className="flex flex-col gap-4">
					<LevelScale level={level} />
					<LevelLine level={level} />
					{!consent && (
						<div className="text-sm">
							<GuaranteeNotes codes={remaining} />
						</div>
					)}
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
						{draft.skipRights && (
							<p>
								{backend === "s3"
									? copy.rights.skippedBody
									: copy.rights.skippedBodyShared}
							</p>
						)}
						<GuaranteeNotes codes={remaining} />
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
