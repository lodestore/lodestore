import { keepPreviousData } from "@tanstack/react-query";
import { CircleAlert, Info, Plus, TriangleAlert, X } from "lucide-react";
import { useEffect, useState } from "react";
import { useAgentPolicy } from "@/api/gen/endpoints";
import { CapabilityChecklist } from "@/components/capability-checklist";
import { PolicyPanel } from "@/components/policy-panel";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
	Field,
	FieldDescription,
	FieldGroup,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";
import {
	accessReady,
	type CheckState,
	checkRows,
	type Draft,
	isExperimental,
	type KeyPair,
	runningRows,
} from "./draft";
import { KeyCard } from "./key-card";
import { SecretInput, StepCard, StepHeading } from "./parts";
import { text } from "./text";

const POLICY_DELAY_MS = 300;
const PLACEHOLDER_BUCKET = "YOUR-BUCKET";

type Patch = (patch: Partial<Draft>) => void;

function useDebounced<T>(value: T, delay: number) {
	const [debounced, setDebounced] = useState(value);
	useEffect(() => {
		const timer = setTimeout(() => setDebounced(value), delay);
		return () => clearTimeout(timer);
	}, [value, delay]);
	return debounced;
}

function TextField({
	id,
	label,
	help,
	value,
	onChange,
	disabled,
}: {
	id: string;
	label: string;
	help?: string;
	value: string;
	onChange: (value: string) => void;
	disabled: boolean;
}) {
	return (
		<Field>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			<Input
				id={id}
				name={id}
				className="font-mono"
				autoComplete="off"
				autoCapitalize="off"
				spellCheck={false}
				disabled={disabled}
				aria-describedby={help ? `${id}-help` : undefined}
				value={value}
				onChange={(event) => onChange(event.target.value)}
			/>
			{help && (
				<FieldDescription id={`${id}-help`} className="wrap-anywhere">
					{help}
				</FieldDescription>
			)}
		</Field>
	);
}

function KeySection({
	prefix,
	legend,
	value,
	onChange,
	disabled,
}: {
	prefix: string;
	legend: string;
	value: KeyPair;
	onChange: (value: KeyPair) => void;
	disabled: boolean;
}) {
	return (
		<FieldSet>
			<FieldLegend variant="label" className="sr-only">
				{legend}
			</FieldLegend>
			<FieldGroup>
				<Field>
					<FieldLabel htmlFor={`${prefix}-key-id`}>
						<span className="sr-only">{`${legend}: `}</span>
						{text.accessKeyId}
					</FieldLabel>
					<Input
						id={`${prefix}-key-id`}
						name={`${prefix}-key-id`}
						className="font-mono"
						autoComplete="off"
						autoCapitalize="off"
						spellCheck={false}
						disabled={disabled}
						value={value.id}
						onChange={(event) => onChange({ ...value, id: event.target.value })}
					/>
				</Field>
				<Field>
					<FieldLabel htmlFor={`${prefix}-key-secret`}>
						<span className="sr-only">{`${legend}: `}</span>
						{text.secretAccessKey}
					</FieldLabel>
					<SecretInput
						id={`${prefix}-key-secret`}
						what={`${legend.toLowerCase()} secret`}
						disabled={disabled}
						value={value.secret}
						onChange={(secret) => onChange({ ...value, secret })}
					/>
				</Field>
			</FieldGroup>
		</FieldSet>
	);
}

function S3Fields({
	draft,
	onChange,
	disabled,
}: {
	draft: Draft;
	onChange: Patch;
	disabled: boolean;
}) {
	return (
		<div className="flex flex-col gap-6">
			<p>{copy.backend.twoKeys}</p>
			<Alert>
				<Info aria-hidden />
				<AlertTitle>{copy.access.whyWriteOnly.title}</AlertTitle>
				<AlertDescription>{copy.access.whyWriteOnly.body}</AlertDescription>
			</Alert>
			<section className="flex flex-col gap-4 rounded-lg border p-6">
				<h3 className="font-semibold">{text.bucket}</h3>
				<FieldGroup>
					<TextField
						id="s3-endpoint"
						label={text.endpoint}
						help={text.endpointHelper}
						value={draft.endpoint}
						onChange={(endpoint) => onChange({ endpoint })}
						disabled={disabled}
					/>
					<TextField
						id="s3-region"
						label={text.region}
						help={text.regionHelper}
						value={draft.region}
						onChange={(region) => onChange({ region })}
						disabled={disabled}
					/>
					<TextField
						id="s3-bucket"
						label={text.bucket}
						value={draft.bucket}
						onChange={(bucket) => onChange({ bucket })}
						disabled={disabled}
					/>
					<TextField
						id="s3-prefix"
						label={text.prefix}
						help={text.prefixHelper}
						value={draft.prefix}
						onChange={(prefix) => onChange({ prefix })}
						disabled={disabled}
					/>
					<Field orientation="horizontal">
						<Switch
							id="s3-path-style"
							disabled={disabled}
							checked={draft.pathStyle ?? draft.endpoint.trim() !== ""}
							onCheckedChange={(pathStyle) => onChange({ pathStyle })}
						/>
						<FieldLabel htmlFor="s3-path-style">{text.pathStyle}</FieldLabel>
					</Field>
				</FieldGroup>
			</section>
			<div className="grid gap-6 xl:grid-cols-2">
				<KeyCard
					number={1}
					title={copy.access.serverKey.title}
					role={copy.access.serverKey.body}
				>
					<KeySection
						prefix="server"
						legend={text.serverKey}
						value={draft.serverKey}
						onChange={(serverKey) => onChange({ serverKey })}
						disabled={disabled}
					/>
				</KeyCard>
				<KeyCard
					number={2}
					title={copy.access.agentKey.title}
					role={copy.access.agentKey.body}
					badge={copy.access.writeOnlyBadge}
				>
					<KeySection
						prefix="agent"
						legend={text.agentKey}
						value={draft.agentKey}
						onChange={(agentKey) => onChange({ agentKey })}
						disabled={disabled}
					/>
				</KeyCard>
			</div>
			<PolicyColumn draft={draft} />
		</div>
	);
}

function ExperimentalFields({
	draft,
	onChange,
	disabled,
}: {
	draft: Draft;
	onChange: Patch;
	disabled: boolean;
}) {
	const [shown, setShown] = useState(false);
	const rows = draft.options;
	const set = (index: number, patch: Partial<(typeof rows)[number]>) =>
		onChange({
			options: rows.map((row, i) => (i === index ? { ...row, ...patch } : row)),
		});

	return (
		<>
			<Alert className="text-warning">
				<TriangleAlert aria-hidden />
				<AlertTitle>{text.experimentalHeading}</AlertTitle>
				<AlertDescription>{text.experimentalBody}</AlertDescription>
			</Alert>
			<FieldGroup>
				{rows.map((row, index) => (
					<div key={index} className="flex items-end gap-2">
						<Field>
							<FieldLabel htmlFor={`option-key-${index}`}>
								{text.optionKey}
							</FieldLabel>
							<Input
								id={`option-key-${index}`}
								className="font-mono"
								autoComplete="off"
								autoCapitalize="off"
								spellCheck={false}
								disabled={disabled}
								value={row.key}
								onChange={(event) => set(index, { key: event.target.value })}
							/>
						</Field>
						<Field>
							<FieldLabel htmlFor={`option-value-${index}`}>
								{text.optionValue}
							</FieldLabel>
							<Input
								id={`option-value-${index}`}
								className="font-mono"
								type={shown ? "text" : "password"}
								autoComplete="off"
								spellCheck={false}
								disabled={disabled}
								value={row.value}
								onChange={(event) => set(index, { value: event.target.value })}
							/>
						</Field>
						<Button
							type="button"
							variant="ghost"
							size="icon"
							aria-label={text.removeOption}
							disabled={disabled || rows.length === 1}
							onClick={() =>
								onChange({ options: rows.filter((_, i) => i !== index) })
							}
						>
							<X aria-hidden />
						</Button>
					</div>
				))}
				<div className="flex flex-wrap gap-2">
					<Button
						type="button"
						variant="secondary"
						size="sm"
						disabled={disabled}
						onClick={() =>
							onChange({ options: [...rows, { key: "", value: "" }] })
						}
					>
						<Plus aria-hidden />
						{text.addOption}
					</Button>
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={() => setShown((v) => !v)}
					>
						{shown ? text.hideValues : text.showValues}
					</Button>
				</div>
			</FieldGroup>
		</>
	);
}

function PolicyColumn({ draft }: { draft: Draft }) {
	const target = useDebounced(
		{ bucket: draft.bucket.trim(), prefix: draft.prefix.trim() },
		POLICY_DELAY_MS,
	);
	// GET /api/v1/agent-policy
	const policy = useAgentPolicy(
		{
			bucket: target.bucket || PLACEHOLDER_BUCKET,
			...(target.prefix ? { prefix: target.prefix } : {}),
		},
		{
			query: {
				placeholderData: keepPreviousData,
				staleTime: 60_000,
			},
		},
	);
	return (
		<div className="min-w-0">
			<PolicyPanel
				iamJson={policy.data?.iam_json}
				minioCommands={policy.data?.minio_commands}
				loading={policy.isPending}
			/>
			{policy.isError && (
				<p role="alert" className="mt-2 text-sm text-destructive">
					{copy.errors.submit("load the policy")}
				</p>
			)}
		</div>
	);
}

export function AccessStep({
	draft,
	onChange,
	check,
	onCheck,
	onBack,
	onContinue,
}: {
	draft: Draft;
	onChange: Patch;
	check: CheckState;
	onCheck: () => void;
	onBack: () => void;
	onContinue: () => void;
}) {
	const running = check.phase === "running";
	const fresh = check.phase === "done" && !check.stale;
	const blocked = fresh && check.result.blocking;
	const existing = check.phase === "done" && check.result.existing_repository;

	return (
		<StepCard
			onBack={onBack}
			backDisabled={running}
			actions={
				fresh && (
					<>
						{blocked && (
							<p
								id="continue-blocked"
								className="text-sm text-muted-foreground"
							>
								{copy.check.blockedContinue}
							</p>
						)}
						<Button
							type="button"
							disabled={blocked}
							aria-describedby={blocked ? "continue-blocked" : undefined}
							onClick={onContinue}
						>
							{copy.wizard.continueToKey}
						</Button>
					</>
				)
			}
		>
			<StepHeading>{text.stepHeading.access}</StepHeading>
			{draft.backend === "s3" && (
				<S3Fields draft={draft} onChange={onChange} disabled={running} />
			)}
			{draft.backend === "local" && (
				<FieldGroup>
					<TextField
						id="local-path"
						label={text.path}
						help={text.pathHelper}
						value={draft.path}
						onChange={(path) => onChange({ path })}
						disabled={running}
					/>
				</FieldGroup>
			)}
			{isExperimental(draft.backend) && (
				<ExperimentalFields
					draft={draft}
					onChange={onChange}
					disabled={running}
				/>
			)}
			<section className="flex flex-col gap-4">
				<div className="flex flex-col gap-1">
					<h3 className="font-semibold">{copy.access.verify.title}</h3>
					<p className="text-sm text-muted-foreground">
						{copy.access.verify.body}
					</p>
				</div>
				<div>
					<Button
						type="button"
						size="lg"
						variant={fresh ? "outline" : "default"}
						disabled={running || !accessReady(draft)}
						onClick={onCheck}
					>
						{running && <Spinner />}
						{running ? copy.check.running : copy.check.button}
					</Button>
				</div>
				{check.phase === "error" && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertDescription>{check.message}</AlertDescription>
					</Alert>
				)}
				{existing && (
					<Alert>
						<Info aria-hidden />
						<AlertTitle>{copy.existingRepo.heading}</AlertTitle>
						<AlertDescription>{copy.existingRepo.body}</AlertDescription>
					</Alert>
				)}
				{(check.phase === "running" || check.phase === "done") && (
					<Card size="sm">
						<CardContent className="wrap-anywhere">
							{check.phase === "running" ? (
								<CapabilityChecklist rows={runningRows()} />
							) : (
								<CapabilityChecklist
									rows={checkRows(check.result, draft.backend, check.stale)}
									stale={check.stale}
								/>
							)}
						</CardContent>
					</Card>
				)}
			</section>
		</StepCard>
	);
}
