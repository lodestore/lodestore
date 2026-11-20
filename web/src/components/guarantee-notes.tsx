import { copy } from "@/copy";

export const WEAK_LEVEL = "host_can_overwrite_or_delete";
export const UNCHECKED_LEVEL = "unchecked";

export function levelName(level: string) {
	return (copy.guarantee.levels as Record<string, string>)[level] ?? level;
}

export function GuaranteeNotes({ codes }: { codes: readonly string[] }) {
	if (codes.length === 0) return null;
	const notes = copy.guarantee.notes as Record<string, string>;
	return (
		<ul className="list-disc pl-5">
			{codes.map((code) => (
				<li key={code}>{notes[code] ?? copy.guarantee.unknownNote}</li>
			))}
		</ul>
	);
}
