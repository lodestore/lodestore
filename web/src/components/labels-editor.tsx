import { CircleAlert, Plus, X } from "lucide-react";
import { useId } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { copy } from "@/copy";
import { type LabelRow, MAX_LABELS, validateLabels } from "@/lib/labels";

const BLANK: LabelRow = { key: "", value: "" };

export function LabelsEditor({
	value,
	onChange,
}: {
	value: LabelRow[];
	onChange: (rows: LabelRow[]) => void;
}) {
	const id = useId();
	const rows = value.length > 0 ? value : [BLANK];
	const { rowErrors, formError } = validateLabels(rows);

	const update = (i: number, patch: Partial<LabelRow>) =>
		onChange(rows.map((row, j) => (j === i ? { ...row, ...patch } : row)));
	const remove = (i: number) => onChange(rows.filter((_, j) => j !== i));

	return (
		<div className="flex flex-col gap-2">
			{rows.map((row, i) => {
				const error = rowErrors[i];
				const errorId = `${id}-error-${i}`;
				const describedBy = error ? errorId : undefined;
				return (
					<div key={i} className="flex flex-col gap-1">
						<div className="flex items-center gap-2">
							<Input
								aria-label={copy.labels.key}
								placeholder={copy.labels.key}
								value={row.key}
								autoFocus={value.length === 0 && i === 0}
								aria-invalid={error?.field === "key"}
								aria-describedby={
									error?.field === "key" ? describedBy : undefined
								}
								onChange={(e) => update(i, { key: e.target.value })}
							/>
							<Input
								aria-label={copy.labels.value}
								placeholder={copy.labels.value}
								value={row.value}
								aria-invalid={error?.field === "value"}
								aria-describedby={
									error?.field === "value" ? describedBy : undefined
								}
								onChange={(e) => update(i, { value: e.target.value })}
							/>
							<Button
								type="button"
								variant="ghost"
								size="icon"
								aria-label={copy.labels.remove}
								onClick={() => remove(i)}
							>
								<X aria-hidden />
							</Button>
						</div>
						{error && (
							<p
								id={errorId}
								className="flex items-start gap-1 text-xs text-destructive"
							>
								<CircleAlert aria-hidden className="mt-1 size-3 shrink-0" />
								{error.message}
							</p>
						)}
					</div>
				);
			})}
			{formError && (
				<p
					role="alert"
					className="flex items-start gap-1 text-xs text-destructive"
				>
					<CircleAlert aria-hidden className="mt-1 size-3 shrink-0" />
					{formError}
				</p>
			)}
			<Button
				type="button"
				variant="ghost"
				size="sm"
				className="self-start"
				disabled={rows.length >= MAX_LABELS}
				onClick={() => onChange([...rows, BLANK])}
			>
				<Plus aria-hidden />
				{copy.labels.add}
			</Button>
		</div>
	);
}
