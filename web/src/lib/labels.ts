import { copy } from "@/copy";

export const MAX_LABELS = 20;
const MAX_LEN = 63;
const KEY_RE = /^[A-Za-z0-9._-]+$/;
const VALUE_RE = /^[\p{L}\p{N}_.-]*$/u;

export type LabelRow = { key: string; value: string };
export type RowError = { field: "key" | "value"; message: string };
export type LabelsValidation = {
	valid: boolean;
	rowErrors: (RowError | null)[];
	formError: string | null;
};

export function validateLabels(rows: LabelRow[]): LabelsValidation {
	const seen = new Set<string>();
	let used = 0;
	const rowErrors = rows.map((row): RowError | null => {
		if (row.key === "" && row.value === "") return null;
		used += 1;
		const fail = (field: RowError["field"], detail = ""): RowError => ({
			field,
			message: `${copy.labels.validation}${detail}`,
		});
		if (row.key.length > MAX_LEN || !KEY_RE.test(row.key)) return fail("key");
		if ([...row.value].length > MAX_LEN || !VALUE_RE.test(row.value))
			return fail("value");
		if (seen.has(row.key)) return fail("key", ` ${copy.labels.duplicateKey}`);
		seen.add(row.key);
		return null;
	});
	const formError =
		used > MAX_LABELS
			? `${copy.labels.validation} ${copy.labels.tooMany}`
			: null;
	return {
		valid: formError === null && rowErrors.every((e) => e === null),
		rowErrors,
		formError,
	};
}
