import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { copy } from "@/copy";
import {
	type CheckState,
	connectionReady,
	type Draft,
	endpointOf,
	hostKeyScope,
	needsConsent,
	type PresetInfo,
	placeholdersOf,
} from "./draft";
import { HostKeyConfirm } from "./host-key-confirm";
import {
	KeyFields,
	PrivateKeyField,
	SecretField,
	StepCard,
	StepHeading,
	TextField,
} from "./parts";
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
							help={
								preset.id === "minio"
									? copy.connection.endpointHelperMinio
									: copy.connection.endpointHelper
							}
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
			<ServerKeyCard>
				<KeyFields
					prefix="server"
					legend={text.serverKey}
					keyId={draft.serverKey.id}
					secret={draft.serverKey.secret}
					onKeyId={(id) => onChange({ serverKey: { ...draft.serverKey, id } })}
					onSecret={(secret) =>
						onChange({ serverKey: { ...draft.serverKey, secret } })
					}
					disabled={disabled}
				/>
			</ServerKeyCard>
		</div>
	);
}

function ServerKeyCard({ children }: { children: ReactNode }) {
	return (
		<Card>
			<CardContent className="flex flex-col gap-4">
				<div className="flex flex-col gap-1">
					<h3 className="font-semibold">{copy.connection.serverKeyTitle}</h3>
					<p className="text-sm text-muted-foreground">
						{copy.connection.serverKeyBody}
					</p>
				</div>
				{children}
			</CardContent>
		</Card>
	);
}

function GcsFields({
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
			<FieldGroup>
				<TextField
					id="gcs-bucket"
					label={text.bucket}
					value={draft.bucket}
					onChange={(bucket) => onChange({ bucket })}
					disabled={disabled}
				/>
				<TextField
					id="gcs-prefix"
					label={copy.connection.folder}
					help={text.prefixHelper}
					value={draft.prefix}
					onChange={(prefix) => onChange({ prefix })}
					disabled={disabled}
				/>
			</FieldGroup>
			<ServerKeyCard>
				<SecretField
					id="server-credential"
					label={copy.connection.gcsCredential}
					help={copy.connection.gcsCredentialHelper}
					value={draft.serverCredential}
					onChange={(serverCredential) => onChange({ serverCredential })}
					disabled={disabled}
				/>
			</ServerKeyCard>
		</div>
	);
}

function SftpFields({
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
	const boxed = preset.endpoint_template !== null;
	const scope = hostKeyScope(draft);
	const canGet =
		scope.endpoint !== "" &&
		placeholdersOf(preset.endpoint_template).every(
			(name) => (draft.templateValues[name] ?? "").trim() !== "",
		);
	return (
		<div className="flex flex-col gap-6">
			<FieldGroup className="md:grid md:grid-cols-2">
				{boxed ? (
					<div className="flex flex-col gap-2">
						<TextField
							id="sftp-user"
							label={copy.connection.placeholder.user}
							value={draft.templateValues.user ?? ""}
							onChange={(user) =>
								onChange({
									templateValues: { ...draft.templateValues, user },
								})
							}
							disabled={disabled}
						/>
						<p className="font-mono text-sm wrap-anywhere text-muted-foreground">
							{scope.endpoint}
						</p>
					</div>
				) : (
					<TextField
						id="sftp-address"
						label={copy.connection.address}
						help={copy.connection.addressHelper}
						value={draft.endpoint}
						onChange={(endpoint) => onChange({ endpoint })}
						disabled={disabled}
					/>
				)}
				<TextField
					id="sftp-directory"
					label={copy.connection.directory}
					help={copy.connection.directoryHelper}
					value={draft.prefix}
					onChange={(prefix) => onChange({ prefix })}
					disabled={disabled}
				/>
			</FieldGroup>
			<HostKeyConfirm
				endpoint={scope.endpoint}
				root={scope.root}
				canGet={canGet && !disabled}
				hostKey={draft.hostKey}
				onConfirm={(hostKey) => onChange({ hostKey })}
			/>
			<ServerKeyCard>
				<FieldGroup>
					{!boxed && (
						<TextField
							id="server-user"
							label={copy.connection.user}
							value={draft.serverUser}
							onChange={(serverUser) => onChange({ serverUser })}
							disabled={disabled}
						/>
					)}
					<PrivateKeyField
						id="server-private-key"
						label={copy.connection.privateKey}
						value={draft.serverPrivateKey}
						onChange={(serverPrivateKey) => onChange({ serverPrivateKey })}
						disabled={disabled}
					/>
				</FieldGroup>
			</ServerKeyCard>
		</div>
	);
}

function AzureFields({
	draft,
	onChange,
	disabled,
}: {
	draft: Draft;
	onChange: Patch;
	disabled: boolean;
}) {
	const sas = draft.azureCredentialKind === "sas";
	return (
		<div className="flex flex-col gap-6">
			<FieldGroup>
				<TextField
					id="azure-account"
					label={copy.connection.accountName}
					value={draft.accountName}
					onChange={(accountName) => onChange({ accountName })}
					disabled={disabled}
				/>
				<TextField
					id="azure-container"
					label={copy.connection.container}
					value={draft.container}
					onChange={(container) => onChange({ container })}
					disabled={disabled}
				/>
				<TextField
					id="azure-prefix"
					label={copy.connection.folder}
					help={copy.connection.azurePrefixHelper}
					value={draft.prefix}
					onChange={(prefix) => onChange({ prefix })}
					disabled={disabled}
				/>
				<TextField
					id="azure-endpoint"
					label={text.endpoint}
					help={copy.connection.azureEndpointHelper}
					value={draft.endpoint}
					onChange={(endpoint) => onChange({ endpoint })}
					disabled={disabled}
				/>
			</FieldGroup>
			<ServerKeyCard>
				<ToggleGroup
					type="single"
					variant="outline"
					aria-label={copy.connection.credentialKind}
					value={draft.azureCredentialKind}
					onValueChange={(kind) => {
						if (kind === "account_key" || kind === "sas") {
							onChange({ azureCredentialKind: kind, serverCredential: "" });
						}
					}}
				>
					<ToggleGroupItem value="account_key">
						{copy.connection.accountKey}
					</ToggleGroupItem>
					<ToggleGroupItem value="sas">
						{copy.connection.serverSas}
					</ToggleGroupItem>
				</ToggleGroup>
				<SecretField
					id="server-credential"
					label={sas ? copy.connection.serverSas : copy.connection.accountKey}
					value={draft.serverCredential}
					onChange={(serverCredential) => onChange({ serverCredential })}
					disabled={disabled}
				/>
			</ServerKeyCard>
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
				<>
					{preset.backend === "gcs" && (
						<GcsFields draft={draft} onChange={onChange} disabled={false} />
					)}
					{preset.backend === "sftp" && (
						<SftpFields
							draft={draft}
							preset={preset}
							onChange={onChange}
							disabled={false}
						/>
					)}
					{preset.backend === "azblob" && (
						<AzureFields draft={draft} onChange={onChange} disabled={false} />
					)}
					{preset.backend === "s3" && (
						<S3Fields
							draft={draft}
							preset={preset}
							onChange={onChange}
							disabled={false}
						/>
					)}
				</>
			)}
		</StepCard>
	);
}
