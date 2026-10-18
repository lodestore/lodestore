import { CircleAlert } from "lucide-react";
import { useId } from "react";
import type { Problem } from "@/api/gen/model";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";

export type FieldProblems = ReadonlyMap<string, string>;

export function problemsByPath(problems: Problem[]): FieldProblems {
	const out = new Map<string, string>();
	for (const { path, message } of problems) {
		if (path != null && !out.has(path)) out.set(path, message);
	}
	return out;
}

export function ErrorText({ id, message }: { id: string; message?: string }) {
	if (!message) return null;
	return (
		<p id={id} className="flex items-start gap-1 text-xs text-destructive">
			<CircleAlert aria-hidden className="mt-1 size-3 shrink-0" />
			{message}
		</p>
	);
}

export function RowField({
	label,
	error,
	helper,
	className,
	children,
}: {
	label: string;
	error?: string;
	helper?: string;
	className?: string;
	children: (props: {
		id: string;
		"aria-invalid": boolean | undefined;
		"aria-describedby": string | undefined;
	}) => React.ReactNode;
}) {
	const id = useId();
	const describedBy = [error && `${id}-error`, helper && `${id}-help`]
		.filter(Boolean)
		.join(" ");
	return (
		<Field data-invalid={error ? true : undefined} className={className}>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			{children({
				id,
				"aria-invalid": error ? true : undefined,
				"aria-describedby": describedBy || undefined,
			})}
			{helper && (
				<FieldDescription id={`${id}-help`}>{helper}</FieldDescription>
			)}
			<ErrorText id={`${id}-error`} message={error} />
		</Field>
	);
}
