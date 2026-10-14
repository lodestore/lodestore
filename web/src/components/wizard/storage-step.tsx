import { ChevronDown } from "lucide-react";
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
					<BackendCard backend="s3" description={text.backendCards.s3} />
					<BackendCard backend="local" description={text.backendCards.local} />
					<Collapsible defaultOpen={isExperimental(draft.backend)}>
						<CollapsibleTrigger asChild>
							<Button variant="ghost" size="sm" className="-ml-2">
								<ChevronDown aria-hidden />
								{text.moreBackends}
							</Button>
						</CollapsibleTrigger>
						<CollapsibleContent className="mt-2 flex flex-col gap-3">
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
