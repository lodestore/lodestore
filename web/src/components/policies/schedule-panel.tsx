import { ChevronDown, Info } from "lucide-react";
import { useId, useState } from "react";
import type { DstNote, ScheduleInfo } from "@/api/gen/model";
import {
	ErrorText,
	type FieldProblems,
	RowField,
} from "@/components/policies/form-field";
import type { ScheduleDraft } from "@/components/policies/policy-form";
import { TimezonePicker } from "@/components/policies/timezone-picker";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Field,
	FieldDescription,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { copy } from "@/copy";
import {
	formatCalendarDay,
	formatDurationShort,
	formatSlot,
} from "@/lib/format";
import { cn } from "@/lib/utils";

const DEFAULT_JITTER = "15m";
const DEFAULT_CATCH_UP = "24h";

function DstLine({ note }: { note: DstNote }) {
	const text = copy.policies.schedule;
	const date = formatCalendarDay(note.date);
	const line =
		note.kind === "repeated"
			? text.repeated(date, note.time)
			: note.runs_at
				? text.skipped(date, note.time, note.runs_at)
				: text.skippedNoRun(date, note.time);
	return (
		<p className="flex items-start gap-2 text-sm wrap-anywhere">
			<Info aria-hidden className="size-4 shrink-0 translate-y-0.5" />
			{line}
		</p>
	);
}

export function NextRuns({
	info,
	pending,
}: {
	info: ScheduleInfo;
	pending?: boolean;
}) {
	const text = copy.policies.schedule;
	return (
		<div
			aria-live="polite"
			className={cn(
				"flex flex-col gap-2 rounded-lg bg-muted p-4",
				pending && "opacity-60",
			)}
		>
			<p className="text-sm font-semibold">{text.nextRuns}</p>
			<p className="text-sm wrap-anywhere">{info.summary}</p>
			<ul className="flex flex-col gap-2 text-sm">
				{info.next_runs.map((ms) => (
					<li key={ms}>
						<time dateTime={new Date(ms).toISOString()}>
							{formatSlot(ms, info.timezone)}
						</time>
					</li>
				))}
			</ul>
			{info.jitter_secs > 0 && (
				<p className="text-sm text-muted-foreground">
					{text.jitterNote(formatDurationShort(info.jitter_secs))}
				</p>
			)}
			{info.dst_notes.map((note) => (
				<DstLine key={`${note.date}-${note.kind}`} note={note} />
			))}
		</div>
	);
}

export function SchedulePanel({
	draft,
	onChange,
	problems,
	info,
	pending,
}: {
	draft: ScheduleDraft;
	onChange: (draft: ScheduleDraft) => void;
	problems: FieldProblems;
	info?: ScheduleInfo | null;
	pending?: boolean;
}) {
	const text = copy.policies.schedule;
	const set = (patch: Partial<ScheduleDraft>) =>
		onChange({ ...draft, ...patch });
	const hasCron = draft.cron.trim() !== "";
	const cronError = problems.get("spec.schedule.cron");
	const zoneError = problems.get("spec.schedule.timezone");
	const jitterError = problems.get("spec.schedule.jitter");
	const catchUpError = problems.get("spec.schedule.catchUpWindow");
	const changed =
		(draft.jitter.trim() !== "" && draft.jitter.trim() !== DEFAULT_JITTER) ||
		(draft.catchUpWindow.trim() !== "" &&
			draft.catchUpWindow.trim() !== DEFAULT_CATCH_UP);
	const [open, setOpen] = useState(changed);
	const zoneId = useId();
	const zoneErrorId = `${zoneId}-error`;

	return (
		<FieldSet className="gap-4">
			<FieldLegend variant="label" className="mb-2">
				{text.legend}
			</FieldLegend>
			<RowField label={text.cron} helper={text.cronHelper} error={cronError}>
				{(a11y) => (
					<Input
						{...a11y}
						className="font-mono"
						autoComplete="off"
						autoCapitalize="off"
						spellCheck={false}
						placeholder={text.cronPlaceholder}
						value={draft.cron}
						onChange={(e) => set({ cron: e.target.value })}
					/>
				)}
			</RowField>
			<Field data-invalid={zoneError ? true : undefined}>
				<FieldLabel htmlFor={zoneId}>{text.timezone}</FieldLabel>
				<TimezonePicker
					id={zoneId}
					value={draft.timezone || "UTC"}
					disabled={!hasCron}
					invalid={zoneError ? true : undefined}
					describedBy={zoneError ? zoneErrorId : undefined}
					onChange={(timezone) => set({ timezone })}
				/>
				<ErrorText id={zoneErrorId} message={zoneError} />
			</Field>
			{hasCron && !cronError && info && (
				<NextRuns info={info} pending={pending} />
			)}
			<Collapsible
				open={open || Boolean(jitterError || catchUpError)}
				onOpenChange={setOpen}
				className="flex flex-col gap-4"
			>
				<CollapsibleTrigger asChild>
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="group/options self-start"
					>
						<ChevronDown
							aria-hidden
							className="transition-transform group-data-[state=closed]/options:-rotate-90"
						/>
						{text.optionsHeader(
							draft.jitter.trim() || DEFAULT_JITTER,
							draft.catchUpWindow.trim() || DEFAULT_CATCH_UP,
						)}
					</Button>
				</CollapsibleTrigger>
				<CollapsibleContent className="flex flex-col gap-4">
					<FieldDescription>
						{hasCron ? text.optionsHelper : text.optionsNeedCron}
					</FieldDescription>
					<RowField label={text.jitter} error={jitterError}>
						{(a11y) => (
							<Input
								{...a11y}
								className="font-mono"
								autoComplete="off"
								spellCheck={false}
								placeholder={text.jitterPlaceholder}
								disabled={!hasCron}
								value={draft.jitter}
								onChange={(e) => set({ jitter: e.target.value })}
							/>
						)}
					</RowField>
					<RowField label={text.catchUp} error={catchUpError}>
						{(a11y) => (
							<Input
								{...a11y}
								className="font-mono"
								autoComplete="off"
								spellCheck={false}
								placeholder={text.catchUpPlaceholder}
								disabled={!hasCron}
								value={draft.catchUpWindow}
								onChange={(e) => set({ catchUpWindow: e.target.value })}
							/>
						)}
					</RowField>
				</CollapsibleContent>
			</Collapsible>
		</FieldSet>
	);
}
