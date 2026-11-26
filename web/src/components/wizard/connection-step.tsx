import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";
import {
	type CheckState,
	connectionReady,
	type Draft,
	endpointOf,
	needsConsent,
	type PresetInfo,
	placeholdersOf,
} from "./draft";
import { KeyFields, StepCard, StepHeading, TextField } from "./parts";
import { ProbeResultPanel } from "./probe-result-panel";
import { text } from "./text";

type Patch = (patch: Partial<Draft>) => void;

function S3Fields({
	draft,
	preset,
	onChange,
	disabled,
}: {
	draft: Draft;
	preset: PresetInfo;
	onChange: Patch;
	disabled: boolean;
}) {
	const names = placeholdersOf(preset.endpoint_template);
	const labelOf = (name: string) => copy.connection.placeholder[name] ?? name;
	return (
		<div className="flex flex-col gap-6">
			<FieldGroup>
				{preset.endpoint_template ? (
					<>
						{names.map((name) => (
							<TextField
								key={name}
								id={`s3-${name}`}
								label={labelOf(name)}
								value={
									name === "region"
										? draft.region
										: (draft.templateValues[name] ?? "")
								}
								onChange={(value) =>
									name === "region"
										? onChange({ region: value })
										: onChange({
												templateValues: {
													...draft.templateValues,
													[name]: value,
												},
											})
								}
								disabled={disabled}
							/>
						))}
						<p className="font-mono text-sm wrap-anywhere text-muted-foreground">
							{endpointOf(draft)}
						</p>
					</>
				) : (
					<>
						<TextField
							id="s3-endpoint"
							label={text.endpoint}
							help={copy.connection.endpointHelper}
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
					</>
				)}
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
						checked={draft.pathStyle ?? false}
						onCheckedChange={(pathStyle) => onChange({ pathStyle })}
					/>
					<FieldLabel htmlFor="s3-path-style">{text.pathStyle}</FieldLabel>
				</Field>
			</FieldGroup>
			<Card>
				<CardContent className="flex flex-col gap-4">
					<div className="flex flex-col gap-1">
						<h3 className="font-semibold">{copy.connection.serverKeyTitle}</h3>
						<p className="text-sm text-muted-foreground">
							{copy.connection.serverKeyBody}
						</p>
					</div>
					<KeyFields
						prefix="server"
						legend={text.serverKey}
						keyId={draft.serverKey.id}
						secret={draft.serverKey.secret}
						onKeyId={(id) =>
							onChange({ serverKey: { ...draft.serverKey, id } })
						}
						onSecret={(secret) =>
							onChange({ serverKey: { ...draft.serverKey, secret } })
						}
						disabled={disabled}
					/>
				</CardContent>
			</Card>
		</div>
	);
}

export function ConnectionStep({
	draft,
	preset,
	onChange,
	check,
	onCheck,
	onBack,
	onContinue,
}: {
	draft: Draft;
	preset: PresetInfo;
	onChange: Patch;
	check: CheckState;
	onCheck: () => void;
	onBack: () => void;
	onContinue: () => void;
}) {
	const running = check.phase === "running";
	const local = preset.backend === "local";
	const fresh = check.phase === "done" && !check.stale;
	const blocked = fresh && check.result.blocking;
	const needsAccept =
		fresh && needsConsent(check.result) && !draft.acceptWeakerGuarantee;
	const ready = connectionReady(draft);

	return (
		<StepCard
			onBack={onBack}
			backDisabled={running}
			actions={
				local ? (
					fresh && (
						<Button
							type="button"
							disabled={blocked || needsAccept}
							aria-describedby={blocked ? "continue-blocked" : undefined}
							onClick={onContinue}
						>
							{copy.wizard.continueToKey}
						</Button>
					)
				) : (
					<Button type="button" disabled={!ready} onClick={onContinue}>
						{copy.wizard.continueToRights}
					</Button>
				)
			}
		>
			<StepHeading>{copy.connection.heading(preset.label)}</StepHeading>
			{local ? (
				<>
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
					<section className="flex flex-col gap-4">
						<div className="flex flex-col gap-1">
							<h3 className="font-semibold">{copy.connection.verifyTitle}</h3>
							<p className="text-sm text-muted-foreground">
								{copy.connection.verifyBody}
							</p>
						</div>
						<div>
							<Button
								type="button"
								size="lg"
								variant={fresh ? "outline" : "default"}
								disabled={running || !ready}
								onClick={onCheck}
							>
								{running && <Spinner />}
								{running ? copy.check.running : copy.check.button}
							</Button>
						</div>
						{check.phase !== "idle" && (
							<ProbeResultPanel
								draft={draft}
								check={check}
								onAccepted={(acceptWeakerGuarantee) =>
									onChange({ acceptWeakerGuarantee })
								}
								onCheckAgain={onCheck}
							/>
						)}
					</section>
				</>
			) : (
				<S3Fields
					draft={draft}
					preset={preset}
					onChange={onChange}
					disabled={false}
				/>
			)}
		</StepCard>
	);
}
