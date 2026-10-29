import { useQueryClient } from "@tanstack/react-query";
import { ChevronDown } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { toast } from "sonner";
import {
	getGetCheckStatusQueryKey,
	usePutCheckSettings,
	useValidateCheckSettings,
} from "@/api/gen/endpoints";
import type {
	CheckLevel,
	CheckLevelInput,
	CheckLevelStatus,
	CheckSettingsCheck,
	CheckSettingsInput,
	CheckStatus,
} from "@/api/gen/model";
import { RowField } from "@/components/policies/form-field";
import { SchedulePanel } from "@/components/policies/schedule-panel";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Field,
	FieldContent,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";

const CHECK_DELAY_MS = 400;
const MAX_FILES = 1000;
const LEVELS: CheckLevel[] = ["structure", "packs", "restore"];

const text = copy.checks.settings;

type LevelDraft = {
	enabled: boolean;
	cron: string;
	timezone: string;
	read: string;
	files: string;
	maxData: string;
};
type Draft = Record<CheckLevel, LevelDraft>;

const fromLevel = ({ settings }: CheckLevelStatus): LevelDraft => ({
	enabled: settings.enabled,
	cron: settings.cron,
	timezone: settings.timezone,
	read: settings.read_per_run,
	files: String(settings.files_per_run),
	maxData: settings.max_data_per_run,
});

const filesValue = (draft: LevelDraft) => {
	const value = draft.files.trim();
	const n = Number(value);
	return /^\d+$/.test(value) && n >= 1 && n <= MAX_FILES ? n : null;
};

function toInput(saved: Draft, draft: Draft): CheckSettingsInput {
	const level = (name: CheckLevel): CheckLevelInput => {
		const d = draft[name];
		return {
			enabled: d.enabled,
			cron: d.cron.trim(),
			timezone: d.timezone.trim() || "UTC",
			...(name === "packs" && { read_per_run: d.read.trim() }),
			...(name === "restore" && {
				files_per_run: filesValue(d) ?? filesValue(saved[name]),
				max_data_per_run: d.maxData.trim(),
			}),
		};
	};
	return {
		structure: level("structure"),
		packs: level("packs"),
		restore: level("restore"),
	};
}

const SIZE_PATHS = new Set(["packs.readPerRun", "restore.maxDataPerRun"]);

function Limits({
	level,
	draft,
	problems,
	update,
}: {
	level: CheckLevel;
	draft: LevelDraft;
	problems: ReadonlyMap<string, string>;
	update: (patch: Partial<LevelDraft>) => void;
}) {
	if (level === "packs") {
		return (
			<RowField
				label={text.readPerRun}
				error={problems.get("packs.readPerRun")}
			>
				{(a11y) => (
					<Input
						{...a11y}
						className="font-mono"
						autoComplete="off"
						autoCapitalize="off"
						spellCheck={false}
						placeholder={text.readPlaceholder}
						value={draft.read}
						onChange={(e) => update({ read: e.target.value })}
					/>
				)}
			</RowField>
		);
	}
	if (level === "restore") {
		return (
			<div className="grid grid-cols-1 gap-4 @min-[560px]:grid-cols-2">
				<RowField
					label={text.filesPerRun}
					error={problems.get("restore.filesPerRun")}
				>
					{(a11y) => (
						<Input
							{...a11y}
							className="font-mono"
							inputMode="numeric"
							autoComplete="off"
							placeholder={text.filesPlaceholder}
							value={draft.files}
							onChange={(e) => update({ files: e.target.value })}
						/>
					)}
				</RowField>
				<RowField
					label={text.maxData}
					helper={text.maxDataHelper}
					error={problems.get("restore.maxDataPerRun")}
					className="col-span-full"
				>
					{(a11y) => (
						<Input
							{...a11y}
							className="font-mono"
							autoComplete="off"
							autoCapitalize="off"
							spellCheck={false}
							placeholder={text.maxDataPlaceholder}
							value={draft.maxData}
							onChange={(e) => update({ maxData: e.target.value })}
						/>
					)}
				</RowField>
			</div>
		);
	}
	return null;
}

function EnabledSwitch({
	checked,
	onChange,
}: {
	checked: boolean;
	onChange: (enabled: boolean) => void;
}) {
	const id = useId();
	return (
		<Field orientation="horizontal">
			<FieldContent>
				<FieldLabel htmlFor={id}>{text.enabled}</FieldLabel>
			</FieldContent>
			<Switch id={id} checked={checked} onCheckedChange={onChange} />
		</Field>
	);
}

function CheckSettingsForm({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: CheckStatus;
}) {
	const client = useQueryClient();
	const levels = new Map(status.levels.map((l) => [l.level, l]));
	const base = Object.fromEntries(
		LEVELS.map((name) => [
			name,
			fromLevel(levels.get(name) as CheckLevelStatus),
		]),
	) as Draft;
	const [edited, setEdited] = useState<Draft | null>(null);
	const [check, setCheck] = useState<CheckSettingsCheck | null>(null);
	const [open, setOpen] = useState(false);
	const [focusTick, setFocusTick] = useState(0);
	const root = useRef<HTMLDivElement>(null);
	const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
	const latest = useRef(0);
	const stale = useRef(false);
	const validate = useValidateCheckSettings();
	const draft = edited ?? base;
	const dirty =
		edited !== null &&
		LEVELS.some((name) =>
			(Object.keys(base[name]) as (keyof LevelDraft)[]).some(
				(key) => edited[name][key] !== base[name][key],
			),
		);

	useEffect(() => () => clearTimeout(timer.current), []);
	useEffect(() => {
		if (focusTick > 0) {
			root.current
				?.querySelector<HTMLElement>('[aria-invalid="true"]')
				?.focus();
		}
	}, [focusTick]);

	function runCheck(next: Draft) {
		const ticket = ++latest.current;
		validate.mutate(
			{ id: repositoryId, data: toInput(base, next) },
			{
				onSuccess: (result) => {
					if (ticket !== latest.current) return;
					stale.current = false;
					setCheck(result);
				},
				onError: () => {
					if (ticket !== latest.current) return;
					stale.current = false;
					setCheck((prev) => prev && { ...prev, errors: [] });
				},
			},
		);
	}

	function update(name: CheckLevel, patch: Partial<LevelDraft>) {
		const next = { ...draft, [name]: { ...draft[name], ...patch } };
		setEdited(next);
		stale.current = true;
		clearTimeout(timer.current);
		timer.current = setTimeout(() => runCheck(next), CHECK_DELAY_MS);
	}

	function discard() {
		clearTimeout(timer.current);
		latest.current += 1;
		stale.current = false;
		setEdited(null);
		setCheck(null);
	}

	const save = usePutCheckSettings({
		mutation: {
			onSuccess: (next) => {
				client.setQueryData(getGetCheckStatusQueryKey(repositoryId), next);
				toast.success(text.saved);
				discard();
			},
			onError: (error) => {
				if (error.status === 422 && error.errors) {
					stale.current = false;
					setCheck({ schedules: check?.schedules ?? [], errors: error.errors });
					setFocusTick((n) => n + 1);
					return;
				}
				if (error.status === 409) {
					void client.invalidateQueries({
						queryKey: getGetCheckStatusQueryKey(repositoryId),
					});
				}
				toast.error(copy.errors.submit(text.saveFailed, error.message));
			},
		},
	});

	const problems = new Map<string, string>();
	if (dirty) {
		for (const { path, message } of check?.errors ?? []) {
			if (problems.has(path)) continue;
			problems.set(
				path,
				SIZE_PATHS.has(path)
					? text.sizeInvalid
					: path === "restore.filesPerRun"
						? text.filesInvalid
						: message,
			);
		}
		if (filesValue(draft.restore) === null) {
			problems.set("restore.filesPerRun", text.filesInvalid);
		}
	}
	const summary = text.headerValues(
		...(LEVELS.map((name) =>
			base[name].enabled
				? (levels.get(name)?.schedule?.summary ?? base[name].cron)
				: text.off,
		) as [string, string, string]),
	);

	function submit() {
		if (save.isPending) return;
		const blocked = stale.current
			? filesValue(draft.restore) === null
			: problems.size > 0;
		if (blocked) {
			setFocusTick((n) => n + 1);
			return;
		}
		clearTimeout(timer.current);
		latest.current += 1;
		save.mutate({ id: repositoryId, data: toInput(base, draft) });
	}

	return (
		<Collapsible
			ref={root}
			open={open || problems.size > 0}
			onOpenChange={setOpen}
			className="flex flex-col gap-4"
		>
			<CollapsibleTrigger asChild>
				<Button
					type="button"
					variant="outline"
					className="group h-auto justify-start self-start py-2 text-left whitespace-normal wrap-anywhere"
				>
					<ChevronDown
						aria-hidden
						className="shrink-0 transition-transform group-data-[state=closed]:-rotate-90"
					/>
					<span>
						<span className="font-semibold">{text.header}</span>
						{summary}
					</span>
				</Button>
			</CollapsibleTrigger>
			<CollapsibleContent className="flex flex-col gap-6">
				{LEVELS.map((name) => {
					const info = dirty
						? check?.schedules.find((s) => s.level === name)?.schedule
						: levels.get(name)?.schedule;
					const panelProblems = new Map<string, string>();
					for (const key of ["cron", "timezone"]) {
						const message = problems.get(`${name}.schedule.${key}`);
						if (message) panelProblems.set(`spec.schedule.${key}`, message);
					}
					return (
						<FieldSet key={name} className="gap-4">
							<FieldLegend className="mb-0">
								{copy.checks.levels[name].name}
							</FieldLegend>
							<EnabledSwitch
								checked={draft[name].enabled}
								onChange={(enabled) => update(name, { enabled })}
							/>
							<SchedulePanel
								showOptions={false}
								showJitter={false}
								cronHelper={text.cronHelper}
								draft={{
									cron: draft[name].cron,
									timezone: draft[name].timezone,
									jitter: "",
									catchUpWindow: "",
									maxAge: "",
								}}
								onChange={({ cron, timezone }) =>
									update(name, { cron, timezone })
								}
								problems={panelProblems}
								info={info}
								pending={validate.isPending}
							/>
							<Limits
								level={name}
								draft={draft[name]}
								problems={problems}
								update={(patch) => update(name, patch)}
							/>
						</FieldSet>
					);
				})}
				<div className="flex flex-wrap items-center gap-2">
					<Button
						type="button"
						variant={dirty ? "default" : "outline"}
						disabled={!dirty}
						aria-disabled={save.isPending || undefined}
						className="aria-disabled:opacity-50"
						onClick={submit}
					>
						{save.isPending && <Spinner />}
						{text.save}
					</Button>
					{dirty && (
						<Button type="button" variant="outline" onClick={discard}>
							{text.discard}
						</Button>
					)}
				</div>
			</CollapsibleContent>
		</Collapsible>
	);
}

export function CheckSettings({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: CheckStatus;
}) {
	if (!LEVELS.every((name) => status.levels.some((l) => l.level === name))) {
		return null;
	}
	return <CheckSettingsForm repositoryId={repositoryId} status={status} />;
}
