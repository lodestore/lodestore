import { useQueryClient } from "@tanstack/react-query";
import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
	getGetStatusQueryKey,
	usePutSettings,
	useValidate,
} from "@/api/gen/endpoints";
import type { GcSettingsCheck, GcStatus } from "@/api/gen/model";
import { problemsByPath, RowField } from "@/components/policies/form-field";
import { SchedulePanel } from "@/components/policies/schedule-panel";
import { settingsInput } from "@/components/repos/reclaim/use-gc-status";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Input } from "@/components/ui/input";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupInput,
	InputGroupText,
} from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const CHECK_DELAY_MS = 400;
const text = copy.gc.settings;

type Draft = {
	cron: string;
	timezone: string;
	pause: string;
	repack: string;
	maxRepack: string;
};

const fromStatus = ({ settings }: GcStatus): Draft => ({
	cron: settings.cron,
	timezone: settings.timezone,
	pause: settings.pause,
	repack: String(settings.repack_below_pct),
	maxRepack: settings.max_repack,
});

const repackPct = (draft: Draft) => {
	const value = draft.repack.trim();
	const pct = Number(value);
	return /^\d+$/.test(value) && pct >= 1 && pct <= 99 ? pct : null;
};

function toInput(status: GcStatus, draft: Draft) {
	return {
		...settingsInput(status, status.enabled),
		cron: draft.cron.trim(),
		timezone: draft.timezone.trim() || "UTC",
		pause: draft.pause.trim(),
		max_repack: draft.maxRepack.trim(),
		repack_below_pct: repackPct(draft) ?? status.settings.repack_below_pct,
	};
}

export function ReclaimSettings({
	repositoryId,
	status,
}: {
	repositoryId: string;
	status: GcStatus;
}) {
	const client = useQueryClient();
	const base = fromStatus(status);
	const [edited, setEdited] = useState<Draft | null>(null);
	const [check, setCheck] = useState<GcSettingsCheck | null>(null);
	const [open, setOpen] = useState(false);
	const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
	const latest = useRef(0);
	const validate = useValidate();
	const draft = edited ?? base;
	const dirty =
		edited !== null &&
		(Object.keys(base) as (keyof Draft)[]).some(
			(key) => edited[key] !== base[key],
		);

	useEffect(() => () => clearTimeout(timer.current), []);

	function runCheck(next: Draft) {
		const ticket = ++latest.current;
		validate.mutate(
			{ id: repositoryId, data: toInput(status, next) },
			{
				onSuccess: (result) => {
					if (ticket === latest.current) setCheck(result);
				},
			},
		);
	}

	function update(patch: Partial<Draft>) {
		const next = { ...draft, ...patch };
		setEdited(next);
		clearTimeout(timer.current);
		timer.current = setTimeout(() => runCheck(next), CHECK_DELAY_MS);
	}

	function discard() {
		clearTimeout(timer.current);
		latest.current += 1;
		setEdited(null);
		setCheck(null);
	}

	const save = usePutSettings({
		mutation: {
			onSuccess: (next) => {
				client.setQueryData(getGetStatusQueryKey(repositoryId), next);
				toast.success(text.saved);
				discard();
			},
			onError: (error) => {
				if (error.status === 422) return runCheck(draft);
				if (error.status === 409) {
					void client.invalidateQueries({
						queryKey: getGetStatusQueryKey(repositoryId),
					});
				}
				toast.error(copy.errors.submit(text.saveFailed, error.message));
			},
		},
	});

	const problems = new Map(
		problemsByPath(dirty ? (check?.problems ?? []) : []),
	);
	if (dirty && repackPct(draft) === null) {
		problems.set("spec.repackBelow", text.repackInvalid);
	}
	const pauseError = problems.get("spec.pause");
	const repackError = problems.get("spec.repackBelow");
	const maxRepackError = problems.get("spec.maxRepack");
	const info = dirty ? check?.schedule : status.schedule;
	const hasCron = status.settings.cron.trim() !== "";
	const summary = hasCron
		? (status.schedule?.summary ?? status.settings.cron)
		: text.manualOnly;

	return (
		<Collapsible
			open={open || problems.size > 0}
			onOpenChange={setOpen}
			className="flex flex-col gap-4"
		>
			<CollapsibleTrigger className="group flex items-start gap-1 text-left text-sm wrap-anywhere outline-none focus-visible:ring-2 focus-visible:ring-ring">
				<ChevronDown
					aria-hidden
					className="size-4 shrink-0 translate-y-0.5 transition-transform group-data-[state=closed]:-rotate-90"
				/>
				<span>
					<span className="font-semibold">{text.header}</span>
					{text.headerValues(
						summary,
						status.pause_effective,
						status.settings.repack_below_pct,
					)}
				</span>
			</CollapsibleTrigger>
			<CollapsibleContent className="flex flex-col gap-6">
				<div className="grid grid-cols-1 gap-4 @min-[560px]:grid-cols-2">
					<div className="col-span-full">
						<SchedulePanel
							showOptions={false}
							showJitter={false}
							cronHelper={text.cronHelper}
							draft={{
								cron: draft.cron,
								timezone: draft.timezone,
								jitter: "",
								catchUpWindow: "",
							}}
							onChange={({ cron, timezone }) => update({ cron, timezone })}
							problems={problems}
							info={info}
							pending={validate.isPending}
						/>
					</div>
					<RowField
						label={text.pause}
						helper={text.pauseHelper(check?.pause_floor ?? status.pause_floor)}
						error={pauseError}
						className="col-span-full"
					>
						{(a11y) => (
							<Input
								{...a11y}
								className="font-mono"
								autoComplete="off"
								autoCapitalize="off"
								spellCheck={false}
								placeholder={text.pausePlaceholder}
								value={draft.pause}
								onChange={(e) => update({ pause: e.target.value })}
							/>
						)}
					</RowField>
					<RowField
						label={text.repack}
						helper={text.repackHelper}
						error={repackError}
					>
						{(a11y) => (
							<InputGroup className="h-10">
								<InputGroupInput
									{...a11y}
									inputMode="numeric"
									autoComplete="off"
									value={draft.repack}
									onChange={(e) => update({ repack: e.target.value })}
								/>
								<InputGroupAddon align="inline-end">
									<InputGroupText>{text.percent}</InputGroupText>
								</InputGroupAddon>
							</InputGroup>
						)}
					</RowField>
					<RowField
						label={text.maxRepack}
						helper={text.maxRepackHelper}
						error={maxRepackError}
					>
						{(a11y) => (
							<Input
								{...a11y}
								className="font-mono"
								autoComplete="off"
								autoCapitalize="off"
								spellCheck={false}
								placeholder={text.maxRepackPlaceholder}
								value={draft.maxRepack}
								onChange={(e) => update({ maxRepack: e.target.value })}
							/>
						)}
					</RowField>
				</div>
				<div className="flex flex-wrap items-center gap-2">
					<Button
						type="button"
						variant={dirty ? "default" : "outline"}
						disabled={!dirty || problems.size > 0}
						aria-disabled={save.isPending || undefined}
						className="aria-disabled:opacity-50"
						onClick={() => {
							if (!save.isPending) {
								save.mutate({
									id: repositoryId,
									data: toInput(status, draft),
								});
							}
						}}
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
