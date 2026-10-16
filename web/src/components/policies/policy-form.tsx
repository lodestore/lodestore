import {
	ArrowDown,
	ArrowUp,
	ChevronDown,
	CircleAlert,
	Plus,
	Search,
	X,
} from "lucide-react";
import { useId, useState } from "react";
import { useListHosts } from "@/api/gen/endpoints";
import type {
	BackupPolicy,
	Hook,
	Problem,
	RepositorySummary,
} from "@/api/gen/model";
import { LabelsEditor } from "@/components/labels-editor";
import { StatusBadge } from "@/components/status-badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Field,
	FieldDescription,
	FieldGroup,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { copy } from "@/copy";
import type { LabelRow } from "@/lib/labels";

const API_VERSION = "lodestore.io/v1alpha1";
const KIND = "BackupPolicy";
export const BLANK_LABEL: LabelRow = { key: "", value: "" };

export type HookDraft = {
	key: number;
	script: string;
	args: string;
	timeout: string;
	command: string | null;
};

export type PolicyDraft = {
	name: string;
	repository: string;
	hostMode: "labels" | "list";
	labels: LabelRow[];
	hosts: string[];
	paths: string[];
	exclude: string;
	oneFileSystem: boolean;
	excludeCaches: boolean;
	before: HookDraft[];
	after: HookDraft[];
};

let hookKeys = 0;

const blankHook = (): HookDraft => ({
	key: ++hookKeys,
	script: "",
	args: "",
	timeout: "",
	command: null,
});

const hookToDraft = (hook: Hook): HookDraft => ({
	key: ++hookKeys,
	script: hook.script ?? "",
	args: (hook.args ?? []).join("\n"),
	timeout: hook.timeout ?? "",
	command: hook.command ?? null,
});

export function draftFromPolicy(policy: BackupPolicy): PolicyDraft {
	const { spec } = policy;
	const labels = Object.entries(spec.selector?.matchLabels ?? {}).map(
		([key, value]) => ({ key, value }),
	);
	const hosts = spec.selector?.hosts ?? [];
	return {
		name: policy.metadata.name,
		repository: spec.repository,
		hostMode: hosts.length > 0 ? "list" : "labels",
		labels: labels.length > 0 ? labels : [BLANK_LABEL],
		hosts,
		paths: spec.paths,
		exclude: (spec.exclude ?? []).join("\n"),
		oneFileSystem: spec.oneFileSystem ?? false,
		excludeCaches: spec.excludeCaches ?? false,
		before: (spec.hooks?.before ?? []).map(hookToDraft),
		after: (spec.hooks?.after ?? []).map(hookToDraft),
	};
}

const lines = (text: string) =>
	text.split(/\r?\n/).filter((line) => line.trim() !== "");

function hookFromDraft(hook: HookDraft): Hook {
	const args = lines(hook.args);
	const timeout = hook.timeout.trim();
	return {
		...(hook.command === null
			? { script: hook.script.trim() }
			: { command: hook.command }),
		...(args.length > 0 && { args }),
		...(timeout !== "" && { timeout }),
	};
}

export function policyFromDraft(draft: PolicyDraft): BackupPolicy {
	return {
		apiVersion: API_VERSION,
		kind: KIND,
		metadata: { name: draft.name.trim() },
		spec: {
			repository: draft.repository,
			selector:
				draft.hostMode === "list"
					? { matchLabels: {}, hosts: draft.hosts }
					: {
							matchLabels: Object.fromEntries(
								draft.labels
									.filter((l) => l.key !== "")
									.map((l) => [l.key, l.value]),
							),
						},
			paths: draft.paths,
			exclude: lines(draft.exclude),
			oneFileSystem: draft.oneFileSystem,
			excludeCaches: draft.excludeCaches,
			hooks: {
				before: draft.before.map(hookFromDraft),
				after: draft.after.map(hookFromDraft),
			},
		},
	};
}

export type FieldProblems = ReadonlyMap<string, string>;

export function problemsByPath(problems: Problem[]): FieldProblems {
	const out = new Map<string, string>();
	for (const { path, message } of problems) {
		if (path != null && !out.has(path)) out.set(path, message);
	}
	return out;
}

function ErrorText({ id, message }: { id: string; message?: string }) {
	if (!message) return null;
	return (
		<p id={id} className="flex items-start gap-1 text-xs text-destructive">
			<CircleAlert aria-hidden className="mt-1 size-3 shrink-0" />
			{message}
		</p>
	);
}

function RowField({
	label,
	error,
	helper,
	children,
}: {
	label: string;
	error?: string;
	helper?: string;
	children: (props: {
		id: string;
		"aria-invalid": boolean | undefined;
		"aria-describedby": string | undefined;
	}) => React.ReactNode;
}) {
	const id = useId();
	const describedBy = [error && `${id}-error`, helper && `${id}-help`]
		.filter(Boolean)
		.join(" ");
	return (
		<Field data-invalid={error ? true : undefined}>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			{children({
				id,
				"aria-invalid": error ? true : undefined,
				"aria-describedby": describedBy || undefined,
			})}
			{helper && (
				<FieldDescription id={`${id}-help`}>{helper}</FieldDescription>
			)}
			<ErrorText id={`${id}-error`} message={error} />
		</Field>
	);
}

function HookRow({
	hook,
	index,
	count,
	at,
	problems,
	onChange,
	onRemove,
	onMove,
}: {
	hook: HookDraft;
	index: number;
	count: number;
	at: string;
	problems: FieldProblems;
	onChange: (hook: HookDraft) => void;
	onRemove: () => void;
	onMove: (by: -1 | 1) => void;
}) {
	const form = copy.policies.form;
	const [touched, setTouched] = useState(
		() => hook.script !== "" || (hook.command ?? "") !== "",
	);
	const set = (patch: Partial<HookDraft>) => onChange({ ...hook, ...patch });
	const shown = (message?: string) => (touched ? message : undefined);
	const isCommand = hook.command !== null;
	return (
		<li
			data-hook={hook.key}
			className="flex flex-col gap-4 rounded-lg border p-4"
		>
			<div className="flex items-center gap-2">
				<span className="text-sm font-semibold tabular-nums">
					{form.step(index + 1)}
				</span>
				<ToggleGroup
					type="single"
					variant="outline"
					size="sm"
					spacing={0}
					aria-label={form.hookType}
					value={isCommand ? "command" : "script"}
					onValueChange={(value) => {
						if (value === "command" && !isCommand) set({ command: "" });
						if (value === "script" && isCommand) set({ command: null });
					}}
				>
					<ToggleGroupItem
						value="script"
						className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
					>
						{form.script}
					</ToggleGroupItem>
					<ToggleGroupItem
						value="command"
						className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
					>
						{form.command}
					</ToggleGroupItem>
				</ToggleGroup>
				<div className="ml-auto flex items-center">
					<Button
						type="button"
						variant="ghost"
						size="icon"
						data-move="up"
						aria-label={form.moveUp(index + 1)}
						disabled={index === 0}
						onClick={() => onMove(-1)}
					>
						<ArrowUp aria-hidden />
					</Button>
					<Button
						type="button"
						variant="ghost"
						size="icon"
						data-move="down"
						aria-label={form.moveDown(index + 1)}
						disabled={index === count - 1}
						onClick={() => onMove(1)}
					>
						<ArrowDown aria-hidden />
					</Button>
					<Button
						type="button"
						variant="ghost"
						size="icon"
						aria-label={form.removeHook}
						onClick={onRemove}
					>
						<X aria-hidden />
					</Button>
				</div>
			</div>
			{isCommand ? (
				<RowField
					label={form.command}
					helper={form.commandHelper}
					error={shown(problems.get(`${at}.command`) ?? problems.get(at))}
				>
					{(a11y) => (
						<Textarea
							{...a11y}
							className="min-h-[2lh] font-mono"
							rows={2}
							spellCheck={false}
							autoCapitalize="off"
							placeholder={form.commandPlaceholder}
							value={hook.command ?? ""}
							onChange={(e) => set({ command: e.target.value })}
							onBlur={() => setTouched(true)}
						/>
					)}
				</RowField>
			) : (
				<RowField
					label={form.script}
					helper={form.scriptHelper}
					error={shown(problems.get(`${at}.script`) ?? problems.get(at))}
				>
					{(a11y) => (
						<Input
							{...a11y}
							className="font-mono"
							autoComplete="off"
							spellCheck={false}
							placeholder={form.scriptPlaceholder}
							value={hook.script}
							onChange={(e) => set({ script: e.target.value })}
							onBlur={() => setTouched(true)}
						/>
					)}
				</RowField>
			)}
			<RowField
				label={form.arguments}
				helper={isCommand ? form.commandArgsHelper : undefined}
				error={problems.get(`${at}.args`)}
			>
				{(a11y) => (
					<Textarea
						{...a11y}
						className="min-h-[2lh] font-mono"
						rows={2}
						spellCheck={false}
						autoCapitalize="off"
						value={hook.args}
						onChange={(e) => set({ args: e.target.value })}
					/>
				)}
			</RowField>
			<RowField
				label={form.timeout}
				helper={form.timeoutHelper}
				error={problems.get(`${at}.timeout`)}
			>
				{(a11y) => (
					<Input
						{...a11y}
						className="font-mono"
						autoComplete="off"
						spellCheck={false}
						placeholder={form.timeoutPlaceholder}
						value={hook.timeout}
						onChange={(e) => set({ timeout: e.target.value })}
					/>
				)}
			</RowField>
		</li>
	);
}

function HookGroup({
	kind,
	hooks,
	problems,
	onChange,
}: {
	kind: "before" | "after";
	hooks: HookDraft[];
	problems: FieldProblems;
	onChange: (hooks: HookDraft[]) => void;
}) {
	const form = copy.policies.form;
	const move = (i: number, by: -1 | 1) => {
		const next = [...hooks];
		[next[i], next[i + by]] = [next[i + by], next[i]];
		onChange(next);
		const key = hooks[i].key;
		setTimeout(() => {
			const row = document.querySelector(`[data-hook="${key}"]`);
			const button =
				row?.querySelector<HTMLButtonElement>(
					`[data-move="${by < 0 ? "up" : "down"}"]:not(:disabled)`,
				) ??
				row?.querySelector<HTMLButtonElement>("[data-move]:not(:disabled)");
			button?.focus();
		}, 0);
	};
	return (
		<section className="flex flex-col gap-2">
			<h4 className="text-sm font-semibold">
				{kind === "before" ? form.before : form.after}
			</h4>
			<p className="text-sm text-muted-foreground">
				{kind === "before" ? form.beforeHelper : form.afterHelper}
			</p>
			{hooks.length > 0 && (
				<ol className="flex flex-col gap-2">
					{hooks.map((hook, i) => (
						<HookRow
							key={hook.key}
							hook={hook}
							index={i}
							count={hooks.length}
							at={`spec.hooks.${kind}[${i}]`}
							problems={problems}
							onChange={(next) =>
								onChange(hooks.map((h, j) => (j === i ? next : h)))
							}
							onRemove={() => onChange(hooks.filter((_, j) => j !== i))}
							onMove={(by) => move(i, by)}
						/>
					))}
				</ol>
			)}
			<Button
				type="button"
				variant="outline"
				size="sm"
				className="self-start"
				onClick={() => onChange([...hooks, blankHook()])}
			>
				<Plus aria-hidden />
				{kind === "before" ? form.addBefore : form.addAfter}
			</Button>
		</section>
	);
}

function HostPicker({
	repositoryId,
	value,
	onChange,
}: {
	repositoryId?: string;
	value: string[];
	onChange: (hosts: string[]) => void;
}) {
	const form = copy.policies.form;
	const hosts = useListHosts();
	const [query, setQuery] = useState("");
	const inRepo = (hosts.data ?? []).filter(
		(h) => h.status !== "revoked" && h.repository_id === repositoryId,
	);
	const known = new Set(inRepo.map((h) => h.id));
	const elsewhere = new Map(
		(hosts.data ?? [])
			.filter((h) => h.status !== "revoked")
			.map((h) => [h.id, h.hostname]),
	);
	const unknown = value.filter((id) => !known.has(id));
	const pickedHere = value.filter((id) => known.has(id)).length;
	const needle = query.trim().toLowerCase();
	const shown = inRepo.filter(
		(h) =>
			needle === "" ||
			h.hostname.toLowerCase().includes(needle) ||
			h.labels.some((l) =>
				`${l.key}=${l.value}`.toLowerCase().includes(needle),
			),
	);
	const picked = new Set(value);
	const toggle = (id: string, on: boolean) =>
		onChange(on ? [...value, id] : value.filter((v) => v !== id));

	if (hosts.isPending)
		return <p className="text-sm text-muted-foreground">{form.hostsLoading}</p>;
	if (hosts.isError) {
		return (
			<p role="alert" className="text-sm text-destructive">
				{form.hostsFailed}{" "}
				<Button
					type="button"
					variant="link"
					className="h-auto p-0"
					onClick={() => void hosts.refetch()}
				>
					{copy.errors.retry}
				</Button>
			</p>
		);
	}
	if (inRepo.length === 0 && unknown.length === 0) {
		return (
			<p className="text-sm text-muted-foreground">{form.noHostsInRepo}</p>
		);
	}
	return (
		<div className="flex flex-col gap-2">
			<div className="flex flex-wrap items-center gap-2">
				{(inRepo.length > 6 || query !== "") && (
					<div className="relative min-w-56 flex-1">
						<Search
							aria-hidden
							className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
						/>
						<Input
							type="search"
							aria-label={form.searchHosts}
							placeholder={form.searchHosts}
							value={query}
							className="pl-10"
							onChange={(e) => setQuery(e.target.value)}
						/>
					</div>
				)}
				<span className="text-sm text-muted-foreground">
					{form.pickedCount(pickedHere, inRepo.length)}
				</span>
				<div className="ml-auto flex gap-2">
					<Button
						type="button"
						variant="outline"
						size="sm"
						disabled={shown.every((h) => picked.has(h.id))}
						onClick={() =>
							onChange([
								...value,
								...shown.filter((h) => !picked.has(h.id)).map((h) => h.id),
							])
						}
					>
						{needle ? form.selectShown : form.selectAll}
					</Button>
					<Button
						type="button"
						variant="outline"
						size="sm"
						disabled={value.length === 0}
						onClick={() => onChange([])}
					>
						{form.clearHosts}
					</Button>
				</div>
			</div>
			<ul className="max-h-80 divide-y overflow-y-auto rounded-lg border">
				{shown.map((host) => {
					const id = `pick-${host.id}`;
					return (
						<li key={host.id} className="flex items-center gap-4 px-4 py-2">
							<Checkbox
								id={id}
								checked={picked.has(host.id)}
								onCheckedChange={(on) => toggle(host.id, on === true)}
							/>
							<label
								htmlFor={id}
								className="flex min-w-0 flex-1 cursor-pointer flex-col"
							>
								<span className="truncate font-semibold">{host.hostname}</span>
								{host.labels.length > 0 && (
									<span className="truncate font-mono text-xs text-muted-foreground">
										{host.labels.map((l) => `${l.key}=${l.value}`).join("  ")}
									</span>
								)}
							</label>
							<StatusBadge
								status={host.status === "online" ? "online" : "offline"}
							/>
						</li>
					);
				})}
				{unknown.map((hostId) => {
					const id = `pick-${hostId}`;
					return (
						<li key={hostId} className="flex items-center gap-4 px-4 py-2">
							<Checkbox
								id={id}
								checked
								onCheckedChange={() => toggle(hostId, false)}
							/>
							<label
								htmlFor={id}
								className="flex min-w-0 flex-1 cursor-pointer flex-col"
							>
								<span className="truncate font-semibold">
									{elsewhere.has(hostId)
										? form.otherRepoHost(elsewhere.get(hostId) ?? "")
										: form.unknownHost}
								</span>
								<span className="truncate font-mono text-xs text-muted-foreground">
									{hostId}
								</span>
							</label>
						</li>
					);
				})}
				{shown.length === 0 && unknown.length === 0 && (
					<li className="px-4 py-2 text-sm text-muted-foreground">
						{form.noHostsFound}
					</li>
				)}
			</ul>
		</div>
	);
}

function PathRow({
	path,
	label,
	placeholder,
	removeLabel,
	error,
	errorId,
	onChange,
	onRemove,
}: {
	path: string;
	label: string;
	placeholder: string;
	removeLabel: string;
	error?: string;
	errorId: string;
	onChange: (path: string) => void;
	onRemove: () => void;
}) {
	const shown = path !== "" ? error : undefined;
	return (
		<div className="flex flex-col gap-1">
			<div className="flex items-center gap-2">
				<Input
					aria-label={label}
					className="font-mono"
					autoComplete="off"
					spellCheck={false}
					placeholder={placeholder}
					aria-invalid={shown ? true : undefined}
					aria-describedby={shown ? errorId : undefined}
					value={path}
					onChange={(e) => onChange(e.target.value)}
				/>
				<Button
					type="button"
					variant="ghost"
					size="icon"
					aria-label={removeLabel}
					onClick={onRemove}
				>
					<X aria-hidden />
				</Button>
			</div>
			<ErrorText id={errorId} message={shown} />
		</div>
	);
}

export function PolicyForm({
	draft,
	onChange,
	problems,
	repositories,
	nameLocked,
	nameTaken,
}: {
	draft: PolicyDraft;
	onChange: (draft: PolicyDraft) => void;
	problems: FieldProblems;
	repositories: RepositorySummary[];
	nameLocked: boolean;
	nameTaken: boolean;
}) {
	const form = copy.policies.form;
	const set = (patch: Partial<PolicyDraft>) => onChange({ ...draft, ...patch });
	const pathProblem = (i: number) => problems.get(`spec.paths[${i}]`);
	const excludeProblem = [...problems]
		.filter(([path]) => path.startsWith("spec.exclude"))
		.map(([, message]) => message)
		.join(" ");
	const hasHooks = draft.before.length + draft.after.length > 0;
	const repoId = useId();

	return (
		<form
			className="flex flex-col gap-6"
			noValidate
			onSubmit={(event) => event.preventDefault()}
		>
			<FieldGroup className="gap-6">
				<RowField
					label={form.name}
					helper={nameLocked ? form.nameLocked : form.nameHelper}
					error={
						nameTaken
							? copy.policies.validation.nameTaken(draft.name.trim())
							: problems.get("metadata.name")
					}
				>
					{(a11y) => (
						<Input
							{...a11y}
							autoComplete="off"
							spellCheck={false}
							disabled={nameLocked}
							placeholder={form.namePlaceholder}
							value={draft.name}
							onChange={(e) => set({ name: e.target.value })}
						/>
					)}
				</RowField>

				<Field data-invalid={problems.has("spec.repository") || undefined}>
					<FieldLabel htmlFor={repoId}>{form.repository}</FieldLabel>
					<Select
						value={draft.repository}
						onValueChange={(repository) => set({ repository })}
					>
						<SelectTrigger
							id={repoId}
							className="w-full"
							aria-invalid={problems.has("spec.repository") || undefined}
						>
							<SelectValue placeholder={form.repositoryPlaceholder} />
						</SelectTrigger>
						<SelectContent>
							{repositories.map((repo) => (
								<SelectItem key={repo.id} value={repo.name}>
									{repo.name}
									<span className="text-muted-foreground">{repo.backend}</span>
								</SelectItem>
							))}
						</SelectContent>
					</Select>
					<FieldDescription>{form.repositoryHelper}</FieldDescription>
					<ErrorText
						id={`${repoId}-error`}
						message={problems.get("spec.repository")}
					/>
				</Field>

				<FieldSet className="gap-2">
					<FieldLegend variant="label" className="mb-0">
						{form.hosts}
					</FieldLegend>
					<ToggleGroup
						type="single"
						variant="outline"
						size="sm"
						spacing={0}
						aria-label={form.hostMode}
						className="mb-2"
						value={draft.hostMode}
						onValueChange={(value) => {
							if (value === "labels" || value === "list") {
								set({ hostMode: value });
							}
						}}
					>
						<ToggleGroupItem
							value="labels"
							className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
						>
							{form.byLabels}
						</ToggleGroupItem>
						<ToggleGroupItem
							value="list"
							className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
						>
							{form.pickHosts}
						</ToggleGroupItem>
					</ToggleGroup>
					{draft.hostMode === "labels" ? (
						<>
							<FieldDescription>{form.hostsHelper}</FieldDescription>
							<LabelsEditor
								value={draft.labels}
								onChange={(labels) =>
									set({ labels: labels.length > 0 ? labels : [BLANK_LABEL] })
								}
							/>
							<ErrorText
								id={`${repoId}-labels-error`}
								message={problems.get("spec.selector.matchLabels")}
							/>
						</>
					) : (
						<>
							<FieldDescription>{form.pickHostsHelper}</FieldDescription>
							<HostPicker
								repositoryId={
									repositories.find((r) => r.name === draft.repository)?.id
								}
								value={draft.hosts}
								onChange={(hosts) => set({ hosts })}
							/>
							<ErrorText
								id={`${repoId}-hosts-error`}
								message={
									problems.get("spec.selector") ??
									[...problems]
										.find(([path]) => path.startsWith("spec.selector.hosts"))
										?.at(1)
								}
							/>
						</>
					)}
				</FieldSet>

				<FieldSet className="gap-2">
					<FieldLegend variant="label" className="mb-0">
						{form.paths}
					</FieldLegend>
					<FieldDescription>{form.pathsHelper}</FieldDescription>
					{draft.paths.map((path, i) => (
						<PathRow
							key={i}
							path={path}
							label={form.paths}
							placeholder={form.pathPlaceholder}
							removeLabel={form.removePath}
							error={pathProblem(i)}
							errorId={`${repoId}-path-${i}`}
							onChange={(next) =>
								set({ paths: draft.paths.map((p, j) => (j === i ? next : p)) })
							}
							onRemove={() =>
								set({ paths: draft.paths.filter((_, j) => j !== i) })
							}
						/>
					))}
					<ErrorText
						id={`${repoId}-paths-error`}
						message={problems.get("spec.paths")}
					/>
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="self-start"
						onClick={() => set({ paths: [...draft.paths, ""] })}
					>
						<Plus aria-hidden />
						{form.addPath}
					</Button>
				</FieldSet>

				<RowField
					label={form.exclude}
					helper={form.excludeHelper}
					error={excludeProblem || undefined}
				>
					{(a11y) => (
						<Textarea
							{...a11y}
							className="min-h-[4lh] font-mono"
							rows={4}
							spellCheck={false}
							autoCapitalize="off"
							value={draft.exclude}
							onChange={(e) => set({ exclude: e.target.value })}
						/>
					)}
				</RowField>

				<FieldSet className="gap-4">
					<FieldLegend variant="label" className="mb-0">
						{form.options}
					</FieldLegend>
					<Field orientation="horizontal">
						<Switch
							id={`${repoId}-one-fs`}
							checked={draft.oneFileSystem}
							onCheckedChange={(oneFileSystem) => set({ oneFileSystem })}
						/>
						<div className="flex flex-col gap-1">
							<FieldLabel htmlFor={`${repoId}-one-fs`}>
								{form.oneFileSystem}
							</FieldLabel>
							<FieldDescription>{form.oneFileSystemHelper}</FieldDescription>
						</div>
					</Field>
					<Field orientation="horizontal">
						<Switch
							id={`${repoId}-cache-tag`}
							checked={draft.excludeCaches}
							onCheckedChange={(excludeCaches) => set({ excludeCaches })}
						/>
						<div className="flex flex-col gap-1">
							<FieldLabel htmlFor={`${repoId}-cache-tag`}>
								{form.cacheDirTag}
							</FieldLabel>
							<FieldDescription>{form.cacheDirTagHelper}</FieldDescription>
						</div>
					</Field>
				</FieldSet>

				<Collapsible defaultOpen={hasHooks} className="flex flex-col gap-4">
					<CollapsibleTrigger asChild>
						<Button
							type="button"
							variant="ghost"
							className="group/hooks self-start px-0"
						>
							<ChevronDown
								aria-hidden
								className="transition-transform group-data-[state=closed]/hooks:-rotate-90"
							/>
							{form.hooks}
						</Button>
					</CollapsibleTrigger>
					<CollapsibleContent className="flex flex-col gap-4">
						<p className="text-sm text-muted-foreground">{form.hooksHelper}</p>
						<HookGroup
							kind="before"
							hooks={draft.before}
							problems={problems}
							onChange={(before) => set({ before })}
						/>
						<HookGroup
							kind="after"
							hooks={draft.after}
							problems={problems}
							onChange={(after) => set({ after })}
						/>
					</CollapsibleContent>
				</Collapsible>
			</FieldGroup>
		</form>
	);
}
