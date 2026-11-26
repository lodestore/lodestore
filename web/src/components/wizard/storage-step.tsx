import {
	Check,
	Cloud,
	HardDrive,
	Info,
	KeyRound,
	Lock,
	Server,
	ShieldCheck,
} from "lucide-react";
import { useStoragePresets } from "@/api/gen/endpoints";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import {
	BACKENDS,
	type Draft,
	LOCAL_PRESET,
	NAME_PATTERN,
	type PresetInfo,
} from "./draft";
import { StepCard, StepHeading } from "./parts";
import { text } from "./text";

const NOT_VERIFIED_SHOWN = ["hetzner-object-storage"];
const SKELETON_TILES = 11;
const SKELETON_KEYS = Array.from({ length: SKELETON_TILES }, (_, i) => i);

function MechanismLine({
	preset,
	note,
	verified,
}: {
	preset: PresetInfo;
	note: string;
	verified: boolean;
}) {
	if (preset.backend === "local") {
		return (
			<span className="inline-flex items-center gap-2 text-sm font-normal text-muted-foreground">
				<Info aria-hidden className="size-4" />
				{copy.presets.localMechanism}
			</span>
		);
	}
	const named = copy.presets.mechanism[preset.id];
	if (preset.mechanism === "server_issued") {
		return (
			<span className="inline-flex items-center gap-2 text-sm font-normal">
				<KeyRound aria-hidden className="size-4" />
				{copy.presets.serverIssued}
			</span>
		);
	}
	if (preset.mechanism === "instruction" && named === undefined) {
		return (
			<span className="inline-flex items-center gap-2 text-sm font-normal text-warning">
				<Lock aria-hidden className="size-4" />
				{copy.presets.cannotLimit}
			</span>
		);
	}
	const suffix =
		NOT_VERIFIED_SHOWN.includes(preset.id) && !verified
			? copy.presets.notVerified
			: "";
	return (
		<span className="inline-flex items-center gap-2 text-sm font-normal">
			<ShieldCheck aria-hidden className="size-4" />
			{`${named ?? note}${suffix}`}
		</span>
	);
}

function PresetTile({
	preset,
	note,
	verified,
	selected,
}: {
	preset: PresetInfo;
	note: string;
	verified: boolean;
	selected: boolean;
}) {
	const Icon =
		preset.backend === "local"
			? HardDrive
			: preset.id === "minio"
				? Server
				: Cloud;
	return (
		<FieldLabel
			htmlFor={`preset-${preset.id}`}
			className="relative min-h-28 w-full flex-col items-stretch justify-between gap-2 rounded-lg border-2 p-4 hover:bg-muted/50 has-[[data-state=checked]]:border-primary-strong has-[[data-state=checked]]:bg-primary-subtle has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-ring"
		>
			<span className="flex items-start justify-between gap-2">
				<span className="font-semibold">{preset.label}</span>
				{selected ? (
					<Check
						aria-hidden
						data-slot="preset-selected"
						className="size-6 shrink-0 text-primary-strong"
					/>
				) : (
					<Icon aria-hidden className="size-6 shrink-0 text-muted-foreground" />
				)}
			</span>
			<MechanismLine preset={preset} note={note} verified={verified} />
			<RadioGroupItem
				value={preset.id}
				id={`preset-${preset.id}`}
				className="absolute top-4 right-4 size-6 opacity-0 after:hidden"
			/>
		</FieldLabel>
	);
}

export function StorageStep({
	draft,
	onChange,
	onPreset,
	onContinue,
}: {
	draft: Draft;
	onChange: (patch: Partial<Draft>) => void;
	onPreset: (preset: PresetInfo) => void;
	onContinue: () => void;
}) {
	const nameValid = NAME_PATTERN.test(draft.name);
	const presets = useStoragePresets();
	const tiles = (presets.data ?? []).filter((p) =>
		BACKENDS.some((b) => b === p.backend),
	);
	const choose = (id: string) => {
		const found =
			id === LOCAL_PRESET.id ? LOCAL_PRESET : tiles.find((p) => p.id === id);
		if (found && found.id !== draft.preset?.id) onPreset(found);
	};

	return (
		<StepCard
			actions={
				<Button
					type="button"
					disabled={!nameValid || draft.preset === null}
					onClick={onContinue}
				>
					{copy.wizard.continueToConnection}
				</Button>
			}
		>
			<StepHeading>{text.stepHeading.storage}</StepHeading>
			<FieldGroup>
				<Field data-invalid={!nameValid || undefined}>
					<FieldLabel htmlFor="repository-name">{text.name}</FieldLabel>
					<Input
						id="repository-name"
						name="repository-name"
						autoComplete="off"
						spellCheck={false}
						aria-invalid={!nameValid || undefined}
						aria-describedby="repository-name-help"
						value={draft.name}
						onChange={(event) => onChange({ name: event.target.value })}
					/>
					{nameValid ? (
						<FieldDescription id="repository-name-help">
							{text.nameHelper}
						</FieldDescription>
					) : (
						<FieldError id="repository-name-help">{text.nameHelper}</FieldError>
					)}
				</Field>
				{presets.isError && (
					<Alert variant="destructive" role="alert">
						<AlertTitle>{copy.presets.loadFailed}</AlertTitle>
						<AlertDescription className="flex flex-col items-start gap-2">
							{copy.presets.loadFailedBody}
							<Button
								type="button"
								variant="outline"
								onClick={() => void presets.refetch()}
							>
								{copy.presets.retry}
							</Button>
						</AlertDescription>
					</Alert>
				)}
				<RadioGroup
					aria-label={copy.presets.legend}
					value={draft.preset?.id ?? ""}
					onValueChange={choose}
				>
					<div className="grid gap-4 min-[680px]:grid-cols-2 lg:grid-cols-3">
						{presets.isPending
							? SKELETON_KEYS.map((key) => (
									<Skeleton key={key} className="h-28 w-full rounded-lg" />
								))
							: [
									...tiles.map((p) => (
										<PresetTile
											key={p.id}
											preset={p}
											note={p.note}
											verified={p.verified}
											selected={draft.preset?.id === p.id}
										/>
									)),
									<PresetTile
										key={LOCAL_PRESET.id}
										preset={LOCAL_PRESET}
										note=""
										verified
										selected={draft.preset?.id === LOCAL_PRESET.id}
									/>,
								]}
					</div>
				</RadioGroup>
			</FieldGroup>
		</StepCard>
	);
}
