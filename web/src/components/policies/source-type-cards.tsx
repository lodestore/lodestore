import { RadioGroup } from "radix-ui";
import type { SourceAvailability } from "@/api/gen/model";
import { FieldLegend, FieldSet } from "@/components/ui/field";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import {
	SOURCE_TYPES,
	type SourceType,
	sourceTypeIcon,
	sourceTypeLabel,
} from "@/lib/source-type";

export type SourceTypeState = "loading" | "ready" | "failed";

export function availabilityText(
	type: SourceType,
	entry: SourceAvailability | undefined,
): string {
	const text = copy.policies.form.source;
	if (type === "files") return text.files.availability;
	if (!entry || entry.hosts_with === 0) return text.notFound;
	const found = text.availability(entry.hosts_with, entry.hosts_total);
	const outdated = entry.hosts_agent_outdated;
	if (outdated === 0) return found;
	return `${found} · ${outdated >= entry.hosts_with ? text.tooOldAll : text.tooOldSome(outdated)}`;
}

export function SourceTypeCards({
	value,
	onChange,
	availability,
	state,
	notice,
}: {
	value: SourceType;
	onChange: (type: SourceType) => void;
	availability?: SourceAvailability[];
	state: SourceTypeState;
	notice?: string;
}) {
	const text = copy.policies.form.source;
	const found = new Map((availability ?? []).map((a) => [a.source_type, a]));
	const shown = SOURCE_TYPES.filter(
		(type) =>
			type === "files" ||
			type === value ||
			(state === "ready" && found.has(type)),
	);
	const loading = state === "loading";
	if (!loading && shown.every((type) => type === "files")) return null;

	return (
		<FieldSet className="gap-2">
			<FieldLegend variant="label" className="mb-0">
				{text.label}
			</FieldLegend>
			{loading ? (
				<Skeleton className="h-20" />
			) : (
				<RadioGroup.Root
					aria-label={text.label}
					value={value}
					onValueChange={(next) => onChange(next as SourceType)}
					className="grid grid-cols-[repeat(auto-fill,minmax(200px,1fr))] gap-2"
				>
					{shown.map((type) => {
						const Icon = sourceTypeIcon[type];
						const entry = found.get(type);
						const unavailable =
							type !== value &&
							type !== "files" &&
							(entry?.hosts_with ?? 0) === 0;
						return (
							<RadioGroup.Item
								key={type}
								value={type}
								disabled={unavailable}
								className="group/card flex flex-col items-start gap-1 rounded-lg border border-input bg-card p-4 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:border-dashed disabled:text-muted-foreground aria-checked:border-primary-strong aria-checked:bg-primary-subtle aria-checked:inset-ring-1 aria-checked:inset-ring-primary-strong"
							>
								<span className="flex items-center gap-2 text-sm font-semibold group-aria-checked/card:text-primary-strong">
									<Icon aria-hidden className="size-4 shrink-0" />
									{sourceTypeLabel(type)}
								</span>
								<span className="text-sm text-muted-foreground">
									{availabilityText(type, entry)}
								</span>
							</RadioGroup.Item>
						);
					})}
				</RadioGroup.Root>
			)}
			{notice && <p className="text-sm text-muted-foreground">{notice}</p>}
		</FieldSet>
	);
}
