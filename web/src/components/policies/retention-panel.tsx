import { Infinity as InfinityIcon } from "lucide-react";
import type { ReactNode } from "react";
import { type FieldProblems, RowField } from "@/components/policies/form-field";
import type { RetentionDraft } from "@/components/policies/policy-form";
import { FieldLegend, FieldSet } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { copy } from "@/copy";

const digits = (value: string) => value.replace(/\D/g, "").slice(0, 9);

export const hasRetention = ({ includeStdin: _, ...rules }: RetentionDraft) =>
	Object.values(rules).some((value) => value.trim() !== "");

type CountKey = "keepLast" | "keepDaily" | "keepWeekly" | "keepMonthly";

export function RetentionPanel({
	draft,
	onChange,
	problems,
	children,
}: {
	draft: RetentionDraft;
	onChange: (draft: RetentionDraft) => void;
	problems: FieldProblems;
	children?: ReactNode;
}) {
	const text = copy.policies.retention;
	const set = (patch: Partial<RetentionDraft>) =>
		onChange({ ...draft, ...patch });
	const count = (key: CountKey, label: string) => (
		<RowField label={label} error={problems.get(`spec.retention.${key}`)}>
			{(a11y) => (
				<Input
					{...a11y}
					inputMode="numeric"
					autoComplete="off"
					value={draft[key]}
					onChange={(e) => set({ [key]: digits(e.target.value) })}
				/>
			)}
		</RowField>
	);

	return (
		<FieldSet className="gap-4">
			<FieldLegend variant="label" className="mb-2">
				{text.legend}
			</FieldLegend>
			<p className="text-sm text-muted-foreground">{text.helper}</p>
			{!hasRetention(draft) && (
				<p className="flex items-center gap-2 text-sm">
					<InfinityIcon aria-hidden className="size-4 shrink-0" />
					{text.none}
				</p>
			)}
			<div className="@container">
				<div className="grid grid-cols-2 gap-4 @min-[560px]:grid-cols-3">
					{count("keepLast", text.keepLast)}
					<RowField
						label={text.keepWithin}
						error={problems.get("spec.retention.keepWithin")}
					>
						{(a11y) => (
							<Input
								{...a11y}
								className="font-mono"
								autoComplete="off"
								autoCapitalize="off"
								spellCheck={false}
								placeholder={text.keepWithinPlaceholder}
								value={draft.keepWithin}
								onChange={(e) => set({ keepWithin: e.target.value })}
							/>
						)}
					</RowField>
					{count("keepDaily", text.keepDaily)}
					{count("keepWeekly", text.keepWeekly)}
					{count("keepMonthly", text.keepMonthly)}
				</div>
			</div>
			{children}
		</FieldSet>
	);
}
