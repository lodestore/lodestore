import { keepPreviousData } from "@tanstack/react-query";
import { Check, Info, TriangleAlert } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useAgentPolicy } from "@/api/gen/endpoints";
import { CodeBlock } from "@/components/code-block";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";
import {
	type AgentSecrets,
	agentFilled,
	DEFAULT_AGENT_USER,
	type PresetInfo,
	presetUsesServerKey,
} from "./draft";
import { KeyFields, PrivateKeyField, SecretField, TextField } from "./parts";
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
		<section
			data-done={done || undefined}
			className="flex gap-4 not-first:border-t not-first:pt-6"
		>
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

function useDebounced(value: string, ms: number) {
	const [debounced, setDebounced] = useState(value);
	useEffect(() => {
		const timer = setTimeout(() => setDebounced(value), ms);
		return () => clearTimeout(timer);
	}, [value, ms]);
	return debounced;
}

const NOTE_NEEDS_REJECTED_CONDITION = new Set(["minio", "wasabi"]);

export function Commands({
	preset,
	bucket,
	prefix,
	user,
	variant = "wizard",
	conditionalPassed = false,
}: {
	preset: PresetInfo;
	bucket: string;
	prefix: string;
	user: string;
	variant?: "wizard" | "sheet";
	conditionalPassed?: boolean;
}) {
	const policy = useAgentPolicy(
		{
			preset: preset.id,
			...(bucket ? { bucket } : {}),
			...(prefix ? { prefix } : {}),
			...(user ? { user } : {}),
		},
		{
			query: {
				staleTime: 60_000,
				retry: false,
				placeholderData: keepPreviousData,
			},
		},
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
	const note =
		rights.note === "" ||
		(conditionalPassed &&
			NOTE_NEEDS_REJECTED_CONDITION.has(preset.id)) ? null : variant ===
			"sheet" ? (
			<Alert>
				<Info aria-hidden />
				<AlertDescription>{rights.note}</AlertDescription>
			</Alert>
		) : (
			<p className="text-sm text-muted-foreground">{rights.note}</p>
		);
	if (presetUsesServerKey(preset)) {
		return (
			<Alert className="text-warning">
				<TriangleAlert aria-hidden />
				<AlertTitle>{copy.rights.r2Title}</AlertTitle>
				<AlertDescription className="whitespace-pre-line">
					{rights.text}
				</AlertDescription>
			</Alert>
		);
	}
	if (
		rights.mechanism === "instruction" ||
		rights.mechanism === "server_issued"
	) {
		return (
			<>
				<p className="text-sm whitespace-pre-line">{rights.text}</p>
				{note}
			</>
		);
	}
	return (
		<>
			<CodeBlock text={rights.text} object={copy.rights.commands} />
			{rights.server_text && (
				<div className="flex flex-col gap-2">
					<p className="text-sm font-semibold">{copy.rights.serverToo}</p>
					<CodeBlock text={rights.server_text} object={copy.rights.serverToo} />
				</div>
			)}
			{note}
		</>
	);
}

function AgentFields({
	preset,
	agent,
	onAgent,
	disabled,
}: {
	preset: PresetInfo;
	agent: AgentSecrets;
	onAgent: (agent: AgentSecrets) => void;
	disabled: boolean;
}) {
	switch (preset.backend) {
		case "gcs":
			return (
				<SecretField
					id="agent-credential"
					label={copy.rights.agentCredential}
					value={agent.credential}
					onChange={(credential) => onAgent({ ...agent, credential })}
					disabled={disabled}
				/>
			);
		case "sftp":
			return (
				<>
					<TextField
						id="agent-user"
						label={copy.rights.agentUser}
						placeholder={
							preset.mechanism === "instruction"
								? undefined
								: DEFAULT_AGENT_USER
						}
						value={agent.user}
						onChange={(user) => onAgent({ ...agent, user })}
						disabled={disabled}
					/>
					<PrivateKeyField
						id="agent-private-key"
						label={copy.rights.agentPrivateKey}
						value={agent.privateKey}
						onChange={(privateKey) => onAgent({ ...agent, privateKey })}
						disabled={disabled}
					/>
				</>
			);
		case "azblob":
			return (
				<SecretField
					id="agent-sas"
					label={copy.rights.agentSas}
					value={agent.sas}
					onChange={(sas) => onAgent({ ...agent, sas })}
					disabled={disabled}
				/>
			);
		default:
			return (
				<KeyFields
					prefix="agent"
					legend={text.agentKey}
					keyId={agent.keyId}
					secret={agent.secret}
					onKeyId={(keyId) => onAgent({ ...agent, keyId })}
					onSecret={(secret) => onAgent({ ...agent, secret })}
					disabled={disabled}
				/>
			);
	}
}

export function AgentRightsTasks({
	preset,
	bucket,
	prefix,
	agent,
	onAgent,
	disabled,
	vendsSas = false,
	variant = "wizard",
	conditionalPassed = false,
}: {
	preset: PresetInfo;
	bucket: string;
	prefix: string;
	agent: AgentSecrets;
	onAgent: (agent: AgentSecrets) => void;
	disabled: boolean;
	vendsSas?: boolean;
	variant?: "wizard" | "sheet";
	conditionalPassed?: boolean;
}) {
	const filled = agentFilled(preset, agent);
	const shared = presetUsesServerKey(preset) || vendsSas;
	const user = useDebounced(
		preset.backend === "sftp"
			? agent.user.trim() ||
					(preset.mechanism === "instruction" ? "" : DEFAULT_AGENT_USER)
			: "",
		300,
	);
	return (
		<>
			<Task
				number={1}
				title={copy.rights.create(preset.label)}
				done={filled && !shared}
			>
				{vendsSas ? (
					<p className="text-sm">{copy.rights.azure}</p>
				) : (
					<Commands
						preset={preset}
						bucket={bucket}
						prefix={prefix}
						user={user}
						variant={variant}
						conditionalPassed={conditionalPassed}
					/>
				)}
			</Task>
			{!shared && (
				<Task number={2} title={copy.rights.paste} done={filled}>
					<AgentFields
						preset={preset}
						agent={agent}
						onAgent={onAgent}
						disabled={disabled}
					/>
				</Task>
			)}
		</>
	);
}
