import type { Ref } from "react";
import type { Problem } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Textarea } from "@/components/ui/textarea";
import { copy } from "@/copy";

export function PolicyYamlEditor({
	value,
	onChange,
	problems,
	blocked,
	textareaRef,
}: {
	value: string;
	onChange: (yaml: string) => void;
	problems: Problem[];
	blocked: boolean;
	textareaRef: Ref<HTMLTextAreaElement>;
}) {
	const text = copy.policies.editor;
	return (
		<div className="flex flex-col gap-2">
			<p className="text-sm text-muted-foreground">{text.yamlNote}</p>
			<Textarea
				ref={textareaRef}
				aria-label={text.yamlLabel}
				aria-invalid={problems.length > 0 || undefined}
				className="min-h-[24lh] font-mono"
				rows={24}
				spellCheck={false}
				autoCapitalize="off"
				autoComplete="off"
				value={value}
				onChange={(e) => onChange(e.target.value)}
			/>
			{(problems.length > 0 || blocked) && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{blocked && <p className="font-semibold">{text.yamlBlocked}</p>}
						<ul className="flex flex-col gap-1">
							{problems.map((problem, i) => (
								<li key={i}>
									{problem.line != null
										? text.yamlLine(problem.line, problem.message)
										: problem.message}
								</li>
							))}
						</ul>
					</AlertDescription>
				</Alert>
			)}
		</div>
	);
}
