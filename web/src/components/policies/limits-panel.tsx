import { ChevronDown } from "lucide-react";
import { useState } from "react";
import { type FieldProblems, RowField } from "@/components/policies/form-field";
import type { LimitsDraft } from "@/components/policies/policy-form";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { copy } from "@/copy";

const DEFAULT_CLASS = "default";
const digits = (value: string) => value.replace(/\D/g, "").slice(0, 3);

function summary(draft: LimitsDraft) {
	const text = copy.policies.limits;
	const parts: string[] = [];
	if (draft.uploadRate.trim() !== "") {
		parts.push(text.upload(draft.uploadRate.trim()));
	}
	if (draft.nice !== "") parts.push(text.nice(draft.nice));
	if (draft.ioniceClass === "idle") {
		parts.push(text.io(text.ioKindIdle));
	} else if (draft.ioniceClass === "best-effort") {
		const kind = [text.ioKindBestEffort, draft.ioniceLevel]
			.filter((part) => part !== "")
			.join(" ");
		parts.push(text.io(kind));
	}
	return parts;
}

export function LimitsPanel({
	draft,
	onChange,
	problems,
}: {
	draft: LimitsDraft;
	onChange: (draft: LimitsDraft) => void;
	problems: FieldProblems;
}) {
	const text = copy.policies.limits;
	const set = (patch: Partial<LimitsDraft>) => onChange({ ...draft, ...patch });
	const parts = summary(draft);
	const rateError = problems.get("spec.limits.uploadRate");
	const niceError = problems.get("spec.limits.nice");
	const levelError = problems.get("spec.limits.ioniceLevel");
	const classError = problems.get("spec.limits.ioniceClass");
	const [open, setOpen] = useState(parts.length > 0);
	const failing = Boolean(rateError || niceError || levelError || classError);

	return (
		<Collapsible
			open={open || failing}
			onOpenChange={setOpen}
			className="flex flex-col gap-4"
		>
			<CollapsibleTrigger asChild>
				<Button
					type="button"
					variant="outline"
					size="sm"
					className="group/limits self-start"
				>
					<ChevronDown
						aria-hidden
						className="transition-transform group-data-[state=closed]/limits:-rotate-90"
					/>
					{text.header(parts)}
				</Button>
			</CollapsibleTrigger>
			<CollapsibleContent className="flex flex-col gap-4">
				<RowField
					label={text.uploadRate}
					helper={text.uploadHelper}
					error={rateError}
				>
					{(a11y) => (
						<Input
							{...a11y}
							className="font-mono"
							autoComplete="off"
							autoCapitalize="off"
							spellCheck={false}
							placeholder={text.uploadPlaceholder}
							value={draft.uploadRate}
							onChange={(e) => set({ uploadRate: e.target.value })}
						/>
					)}
				</RowField>
				<RowField
					label={text.niceLabel}
					helper={text.niceHelper}
					error={niceError}
				>
					{(a11y) => (
						<Input
							{...a11y}
							inputMode="numeric"
							autoComplete="off"
							placeholder={text.nicePlaceholder}
							value={draft.nice}
							onChange={(e) => set({ nice: digits(e.target.value) })}
						/>
					)}
				</RowField>
				<RowField
					label={text.ioPriority}
					helper={draft.ioniceClass === "idle" ? text.idleHelper : undefined}
					error={classError}
				>
					{(a11y) => (
						<Select
							value={
								draft.ioniceClass === "" ? DEFAULT_CLASS : draft.ioniceClass
							}
							onValueChange={(value) =>
								set({
									ioniceClass:
										value === "best-effort" || value === "idle" ? value : "",
								})
							}
						>
							<SelectTrigger {...a11y} className="w-full">
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								<SelectItem value={DEFAULT_CLASS}>{text.ioDefault}</SelectItem>
								<SelectItem value="best-effort">{text.ioBestEffort}</SelectItem>
								<SelectItem value="idle">{text.ioIdle}</SelectItem>
							</SelectContent>
						</Select>
					)}
				</RowField>
				{draft.ioniceClass === "best-effort" && (
					<RowField
						label={text.level}
						helper={text.levelHelper}
						error={levelError}
					>
						{(a11y) => (
							<Input
								{...a11y}
								inputMode="numeric"
								autoComplete="off"
								placeholder={text.levelPlaceholder}
								value={draft.ioniceLevel}
								onChange={(e) => set({ ioniceLevel: digits(e.target.value) })}
							/>
						)}
					</RowField>
				)}
			</CollapsibleContent>
		</Collapsible>
	);
}
