import { Check, ChevronDown, Cloud, HardDrive } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Field,
	FieldContent,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
	FieldTitle,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { copy } from "@/copy";
import {
	type Backend,
	type Draft,
	EXPERIMENTAL_BACKENDS,
	isExperimental,
	NAME_PATTERN,
} from "./draft";
import { StepCard, StepHeading } from "./parts";
import { text } from "./text";

function BackendCard({
	backend,
	description,
	experimental = false,
}: {
	backend: Backend;
	description?: string;
	experimental?: boolean;
}) {
	return (
		<FieldLabel htmlFor={`backend-${backend}`}>
			<Field orientation="horizontal">
				<FieldContent>
					<FieldTitle>
						{text.backendNames[backend]}
						{experimental && (
							<Badge variant="outline" className="border-warning text-warning">
								{copy.backend.experimental}
							</Badge>
						)}
					</FieldTitle>
					{description !== undefined && (
						<FieldDescription>{description}</FieldDescription>
					)}
				</FieldContent>
				<RadioGroupItem value={backend} id={`backend-${backend}`} />
			</Field>
		</FieldLabel>
	);
}

const TILE_ICONS = { s3: Cloud, local: HardDrive } as const;

function BackendTile({
	backend,
	description,
	selected,
}: {
	backend: "s3" | "local";
	description: string;
	selected: boolean;
}) {
	const Icon = TILE_ICONS[backend];
	return (
		<FieldLabel
			htmlFor={`backend-${backend}`}
			className="relative min-h-36 w-full flex-col items-start gap-4 rounded-lg border-2 p-6 hover:bg-muted/50 has-[[data-state=checked]]:border-primary-strong has-[[data-state=checked]]:bg-primary-subtle has-[:focus-visible]:ring-2 has-[:focus-visible]:ring-ring"
		>
			<span className="flex size-12 items-center justify-center rounded-md bg-muted">
				<Icon aria-hidden className="size-8" />
			</span>
			<span className="flex flex-col gap-1">
				<span className="font-semibold">{text.backendNames[backend]}</span>
				<span className="text-sm font-normal text-muted-foreground">
					{description}
				</span>
			</span>
			<span
				aria-hidden
				className="absolute top-4 right-4 flex size-6 items-center justify-center rounded-full border"
			>
				{selected && (
					<Check
						data-slot="backend-selected"
						className="size-6 rounded-full bg-primary p-1 text-primary-foreground"
					/>
				)}
			</span>
			<RadioGroupItem
				value={backend}
				id={`backend-${backend}`}
				className="absolute top-4 right-4 size-6 opacity-0 after:hidden"
			/>
		</FieldLabel>
	);
}

export function StorageStep({
	draft,
	onChange,
	onBackend,
	onContinue,
}: {
	draft: Draft;
	onChange: (patch: Partial<Draft>) => void;
	onBackend: (backend: Backend) => void;
	onContinue: () => void;
}) {
	const nameValid = NAME_PATTERN.test(draft.name);

	return (
		<StepCard
			actions={
				<Button type="button" disabled={!nameValid} onClick={onContinue}>
					{copy.wizard.continueToAccess}
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
				<RadioGroup
					aria-label={text.backendLegend}
					value={draft.backend}
					onValueChange={(value) => onBackend(value as Backend)}
				>
					<div className="grid gap-6 md:grid-cols-2">
						<BackendTile
							backend="s3"
							description={text.backendCards.s3}
							selected={draft.backend === "s3"}
						/>
						<BackendTile
							backend="local"
							description={text.backendCards.local}
							selected={draft.backend === "local"}
						/>
					</div>{" "}
					<Collapsible defaultOpen={isExperimental(draft.backend)}>
						<CollapsibleTrigger asChild>
							<Button variant="ghost" size="sm" className="-ml-2">
								<ChevronDown aria-hidden />
								{text.moreBackends}
							</Button>
						</CollapsibleTrigger>
						<CollapsibleContent className="mt-2 flex flex-col gap-4">
							<p className="text-sm text-muted-foreground">
								{text.moreBackendsBody}
							</p>
							{EXPERIMENTAL_BACKENDS.map((backend) => (
								<BackendCard key={backend} backend={backend} experimental />
							))}
						</CollapsibleContent>
					</Collapsible>
				</RadioGroup>
			</FieldGroup>
		</StepCard>
	);
}
