import { CircleAlert, CircleCheck } from "lucide-react";
import type { Ref } from "react";
import type { Problem } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { copy } from "@/copy";
import { errorCount } from "@/lib/plural";
import { cn } from "@/lib/utils";

export type SyncState =
	| { kind: "sync" | "updating" | "failed" }
	| { kind: "errors"; count: number };

function SyncStatus({ state }: { state: SyncState }) {
	const text = copy.policies.sync;
	return (
		<span
			role="status"
			aria-live="polite"
			className={cn(
				"flex items-center gap-1 text-sm",
				state.kind === "sync" && "text-success",
				(state.kind === "errors" || state.kind === "failed") &&
					"text-destructive",
				state.kind === "updating" && "text-muted-foreground",
			)}
		>
			{state.kind === "updating" && <Spinner aria-hidden role="presentation" />}
			{state.kind === "sync" && <CircleCheck aria-hidden className="size-4" />}
			{(state.kind === "errors" || state.kind === "failed") && (
				<CircleAlert aria-hidden className="size-4" />
			)}
			{state.kind === "sync" && text.inSync}
			{state.kind === "updating" && text.updating}
			{state.kind === "failed" && text.failed}
			{state.kind === "errors" && errorCount(state.count)}
		</span>
	);
}

export function PolicyYamlEditor({
	value,
	onChange,
	problems,
	blocked,
	stale,
	sync,
	textareaRef,
	className,
}: {
	value: string;
	onChange: (yaml: string) => void;
	problems: Problem[];
	blocked: boolean;
	stale: boolean;
	sync: SyncState;
	textareaRef: Ref<HTMLTextAreaElement>;
	className?: string;
}) {
	const text = copy.policies.editor;
	return (
		<section
			role="group"
			aria-label={copy.policies.panes.yamlAria}
			className={cn(
				"flex min-w-0 flex-col rounded-lg bg-card text-card-foreground shadow-xs ring-1 ring-foreground/10",
				"@min-[960px]:sticky @min-[960px]:top-24 @min-[960px]:h-[calc(100dvh-12rem)] @min-[960px]:max-h-[720px] @min-[960px]:min-h-[480px]",
				className,
			)}
		>
			<div className="flex h-12 shrink-0 items-center justify-between border-b px-4">
				<h2 className="text-sm font-semibold">{copy.policies.panes.yaml}</h2>
				<SyncStatus state={sync} />
			</div>
			<div className="flex min-h-0 flex-1 flex-col gap-2 p-4">
				<p className="text-sm text-muted-foreground">{text.yamlNote}</p>
				<Textarea
					ref={textareaRef}
					aria-label={text.yamlLabel}
					aria-invalid={problems.length > 0 || undefined}
					className="min-h-[24lh] font-mono @min-[960px]:min-h-0 @min-[960px]:flex-1 @min-[960px]:field-sizing-fixed @min-[960px]:resize-none"
					rows={24}
					spellCheck={false}
					autoCapitalize="off"
					autoComplete="off"
					value={value}
					onChange={(e) => onChange(e.target.value)}
				/>
				{stale && (
					<p className="text-sm text-muted-foreground">{text.yamlStale}</p>
				)}
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
		</section>
	);
}
