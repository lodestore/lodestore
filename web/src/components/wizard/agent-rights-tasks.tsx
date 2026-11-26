import { Check } from "lucide-react";
import type { ReactNode } from "react";
import { ApiError } from "@/api/fetcher";
import { useAgentPolicy } from "@/api/gen/endpoints";
import { CodeBlock } from "@/components/code-block";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";
import type { AgentSecrets, PresetInfo } from "./draft";
import { KeyFields } from "./parts";
import { text } from "./text";

export function Task({
	number,
	title,
	done,
	children,
}: {
	number: number;
	title: string;
	done: boolean;
	children: ReactNode;
}) {
	return (
		<section className="flex gap-4 not-first:border-t not-first:pt-6">
			<span
				aria-hidden
				className={cn(
					"flex size-8 shrink-0 items-center justify-center rounded-full border text-sm font-semibold",
					done
						? "border-primary-strong bg-primary-strong text-card"
						: "bg-card",
				)}
			>
				{done ? <Check className="size-4" /> : number}
			</span>
			<div className="flex min-w-0 flex-1 flex-col gap-4">
				<h3 className="font-semibold">{title}</h3>
				{children}
			</div>
		</section>
	);
}

function Commands({
	preset,
	bucket,
	prefix,
}: {
	preset: PresetInfo;
	bucket: string;
	prefix: string;
}) {
	const policy = useAgentPolicy(
		{
			preset: preset.id,
			bucket,
			...(prefix ? { prefix } : {}),
		},
		{ query: { staleTime: 60_000, retry: false } },
	);
	if (policy.isPending) return <Skeleton className="h-40 w-full" />;
	if (policy.isError) {
		const invalid =
			policy.error instanceof ApiError && policy.error.status === 422;
		return (
			<Alert variant="destructive" role="alert">
				<AlertDescription className="flex flex-col items-start gap-2">
					{invalid ? policy.error.message : copy.rights.loadFailed}
					{!invalid && (
						<Button
							type="button"
							variant="outline"
							onClick={() => void policy.refetch()}
						>
							{copy.rights.retry}
						</Button>
					)}
				</AlertDescription>
			</Alert>
		);
	}
	const rights = policy.data.rights;
	if (!rights) return null;
	return (
		<>
			<CodeBlock text={rights.text} object={copy.rights.commands} />
			{rights.server_text && (
				<div className="flex flex-col gap-2">
					<p className="text-sm font-semibold">{copy.rights.serverToo}</p>
					<CodeBlock text={rights.server_text} object={copy.rights.serverToo} />
				</div>
			)}
			{rights.note !== "" && (
				<p className="text-sm text-muted-foreground">{rights.note}</p>
			)}
		</>
	);
}

export function AgentRightsTasks({
	preset,
	bucket,
	prefix,
	agent,
	onAgent,
	disabled,
}: {
	preset: PresetInfo;
	bucket: string;
	prefix: string;
	agent: AgentSecrets;
	onAgent: (agent: AgentSecrets) => void;
	disabled: boolean;
}) {
	const filled = agent.keyId.trim() !== "" && agent.secret !== "";
	return (
		<>
			<Task number={1} title={copy.rights.create(preset.label)} done={filled}>
				<Commands preset={preset} bucket={bucket} prefix={prefix} />
			</Task>
			<Task number={2} title={copy.rights.paste} done={filled}>
				<KeyFields
					prefix="agent"
					legend={text.agentKey}
					keyId={agent.keyId}
					secret={agent.secret}
					onKeyId={(keyId) => onAgent({ ...agent, keyId })}
					onSecret={(secret) => onAgent({ ...agent, secret })}
					disabled={disabled}
				/>
			</Task>
		</>
	);
}
