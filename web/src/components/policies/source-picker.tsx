import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { Search, TriangleAlert } from "lucide-react";
import { useEffect, useId, useState } from "react";
import { sourceOptions } from "@/api/gen/endpoints";
import type {
	Selector,
	SourceOptions,
	SourceOptionsRequest,
} from "@/api/gen/model";
import { ErrorText } from "@/components/policies/form-field";
import type { SourceDraft } from "@/components/policies/policy-form";
import { Truncated } from "@/components/truncated";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldLegend, FieldSet } from "@/components/ui/field";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupInput,
} from "@/components/ui/input-group";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { copy } from "@/copy";
import { guestCount } from "@/lib/plural";
import { sourceTypeNoun } from "@/lib/source-type";
import { cn } from "@/lib/utils";

const PAGE = 25;
const MAX_TAGS = 20;
const DEBOUNCE_MS = 400;
const WIDE = "hidden @min-[440px]:table-cell";
const PAD = "px-2 @min-[440px]:px-4";
const MODE =
	"px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong";

type Noun = "guest" | "database" | "schema";
type Row = { key: string; label: string; hosts?: string[] };

const text = copy.policies.form.source;

function useSourceOptions(
	request: SourceOptionsRequest | undefined,
	type: string,
) {
	const wanted = request && JSON.stringify({ ...request, source_type: "" });
	const [settled, setSettled] = useState(wanted);
	useEffect(() => {
		const timer = setTimeout(() => setSettled(wanted), DEBOUNCE_MS);
		return () => clearTimeout(timer);
	}, [wanted]);
	const asked: SourceOptionsRequest | undefined =
		settled === undefined
			? undefined
			: { ...JSON.parse(settled), source_type: type };
	return useQuery({
		queryKey: ["policy-source-options", asked],
		queryFn: ({ signal }) =>
			sourceOptions(asked as SourceOptionsRequest, undefined, signal),
		enabled: asked !== undefined,
		retry: false,
		placeholderData: (previous, previousQuery) =>
			(previousQuery?.queryKey[1] as SourceOptionsRequest | undefined)
				?.source_type === type
				? keepPreviousData(previous)
				: undefined,
	});
}

export function SourcePicker({
	value,
	onChange,
	noun,
	repository,
	selector,
	hostsSelected,
	notice,
	error,
}: {
	value: SourceDraft;
	onChange: (value: SourceDraft) => void;
	noun: Noun;
	repository: string;
	selector: Selector;
	hostsSelected: boolean;
	notice?: string;
	error?: string;
}) {
	const errorId = useId();
	const type = value.type;
	const request =
		hostsSelected && repository !== ""
			? { repository, selector, source_type: type }
			: undefined;
	const query = useSourceOptions(request, type);
	const data = request ? query.data : undefined;
	const isGuest = noun === "guest";
	const dbType = type === "mysql" ? "mysql" : "postgres";
	const mixed = value.mixed?.type === type;

	return (
		<FieldSet className="gap-2">
			<FieldLegend variant="label" className="mb-0">
				{isGuest ? text.guests.label : text.databases.label[dbType]}
			</FieldLegend>
			{!request ? (
				<p className="text-sm text-muted-foreground">{text.selectHostsFirst}</p>
			) : (
				<>
					{isGuest ? (
						<GuestModes
							value={value}
							onChange={onChange}
							data={data}
							query={query}
							mixed={mixed}
						/>
					) : (
						<DatabaseModes
							value={value}
							onChange={onChange}
							data={data}
							query={query}
							mixed={mixed}
							dbType={dbType}
						/>
					)}
					{data && <PickerNotes data={data} noun={noun} />}
				</>
			)}
			<ErrorText id={`${errorId}-error`} message={error} />
			{notice && <p className="text-sm text-muted-foreground">{notice}</p>}
		</FieldSet>
	);
}

type QueryState = ReturnType<typeof useSourceOptions>;

function Modes<T extends string>({
	label,
	modes,
	value,
	onChange,
	disabled,
}: {
	label: string;
	modes: [T, string][];
	value: T;
	onChange: (mode: T) => void;
	disabled?: boolean;
}) {
	return (
		<ToggleGroup
			type="single"
			variant="outline"
			size="sm"
			spacing={0}
			aria-label={label}
			disabled={disabled}
			value={value}
			onValueChange={(next) => {
				const mode = modes.find(([m]) => m === next);
				if (mode) onChange(mode[0]);
			}}
		>
			{modes.map(([mode, name]) => (
				<ToggleGroupItem key={mode} value={mode} className={MODE}>
					{name}
				</ToggleGroupItem>
			))}
		</ToggleGroup>
	);
}

function Loading({ query }: { query: QueryState }) {
	if (query.isError) {
		return (
			<Alert variant="destructive" role="alert">
				<AlertDescription className="flex items-center justify-between gap-4">
					{text.loadFailed}
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={() => void query.refetch()}
					>
						{text.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}
	return (
		<div className="flex flex-col gap-2" aria-busy>
			<InputGroup className="h-10" aria-hidden>
				<InputGroupAddon>
					<Search aria-hidden />
				</InputGroupAddon>
				<InputGroupInput
					type="search"
					disabled
					tabIndex={-1}
					className="h-full"
				/>
			</InputGroup>
			<div className="rounded-lg border bg-card">
				{Array.from({ length: 5 }, (_, i) => (
					<div
						key={i}
						aria-hidden
						className="flex h-12 items-center gap-4 border-b px-4 last:border-b-0"
					>
						<Skeleton className="size-4" />
						<Skeleton className="h-4 w-40" />
					</div>
				))}
			</div>
		</div>
	);
}

function GuestModes({
	value,
	onChange,
	data,
	query,
	mixed,
}: {
	value: SourceDraft;
	onChange: (value: SourceDraft) => void;
	data?: SourceOptions;
	query: QueryState;
	mixed: boolean;
}) {
	const { guests } = value;
	const modes = text.guests.modes;
	const kind = value.type === "proxmox-ct" ? "ct" : "vm";
	const setGuests = (patch: Partial<SourceDraft["guests"]>) =>
		onChange({ ...value, guests: { ...guests, ...patch } });
	const rows: Row[] = (data?.guests ?? [])
		.filter((g) => g.kind === kind)
		.map((g) => ({
			key: String(g.vmid),
			label: text.guestRow(g.kind, g.vmid, g.name),
			hosts: g.hosts,
		}));

	return (
		<>
			<Modes
				label={text.guests.label}
				modes={[
					["pick", modes.pick],
					["pool", modes.pool],
					["tags", modes.tags],
				]}
				value={guests.mode}
				disabled={mixed}
				onChange={(mode) => setGuests({ mode })}
			/>
			{mixed ? (
				<p className="text-sm text-muted-foreground">{text.guests.mixed}</p>
			) : !data ? (
				<Loading query={query} />
			) : guests.mode === "pick" ? (
				<PickTable
					key="pick"
					noun="guest"
					searchNoun={sourceTypeNoun(value.type)}
					column={text.guests.guestColumn}
					rows={rows}
					selected={guests.vmids.map(String)}
					missingLabel={(key) => text.guestRow(kind, Number(key))}
					onSelect={(keys) =>
						setGuests({
							vmids: keys.map(Number).sort((a, b) => a - b),
						})
					}
				/>
			) : guests.mode === "pool" ? (
				<PoolSelect
					pools={data.pools}
					value={guests.pool}
					onChange={(pool) => setGuests({ pool })}
				/>
			) : (
				<TagChips
					tags={data.tags}
					value={guests.tags}
					onChange={(tags) => setGuests({ tags })}
				/>
			)}
		</>
	);
}

function DatabaseModes({
	value,
	onChange,
	data,
	query,
	mixed,
	dbType,
}: {
	value: SourceDraft;
	onChange: (value: SourceDraft) => void;
	data?: SourceOptions;
	query: QueryState;
	mixed: boolean;
	dbType: "postgres" | "mysql";
}) {
	const { databases } = value;
	const modes = text.databases.modes;
	const noun = dbType === "mysql" ? "schema" : "database";
	const setDatabases = (patch: Partial<SourceDraft["databases"]>) =>
		onChange({ ...value, databases: { ...databases, ...patch } });
	const rows: Row[] = (data?.databases ?? []).map((d) => ({
		key: d.name,
		label: d.name,
		hosts: d.hosts,
	}));

	return (
		<>
			<Modes
				label={text.databases.label[dbType]}
				modes={[
					["all", modes.all[dbType]],
					["pick", modes.pick[dbType]],
					["except", modes.except[dbType]],
				]}
				value={databases.mode}
				disabled={mixed}
				onChange={(mode) => setDatabases({ mode })}
			/>
			{mixed ? (
				<p className="text-sm text-muted-foreground">{text.databases.mixed}</p>
			) : databases.mode === "all" ? (
				<p className="text-sm text-muted-foreground">
					{text.databases.allHelper[dbType]}
				</p>
			) : !data ? (
				<Loading query={query} />
			) : (
				<PickTable
					key={databases.mode}
					noun={noun}
					column={text.databases.column[dbType]}
					rows={rows}
					excluding={databases.mode === "except"}
					selected={
						databases.mode === "pick" ? databases.pick : databases.except
					}
					missingLabel={(key) => key}
					onSelect={(keys) =>
						setDatabases(
							databases.mode === "pick" ? { pick: keys } : { except: keys },
						)
					}
				/>
			)}
		</>
	);
}

function PickTable({
	noun,
	searchNoun,
	column,
	rows,
	selected,
	excluding = false,
	missingLabel,
	onSelect,
}: {
	noun: Noun;
	searchNoun?: string;
	column: string;
	rows: Row[];
	selected: string[];
	excluding?: boolean;
	missingLabel: (key: string) => string;
	onSelect: (keys: string[]) => void;
}) {
	const id = useId();
	const [search, setSearch] = useState("");
	const [pages, setPages] = useState(1);
	const [focusKey, setFocusKey] = useState<string>();
	const plural = text.plural[noun];
	const known = new Set(rows.map((r) => r.key));
	const missing: Row[] = selected
		.filter((key) => !known.has(key))
		.map((key) => ({ key, label: missingLabel(key) }));
	const needle = search.trim().toLowerCase();
	const all = [...missing, ...rows];
	const filtered = all.filter(
		(r) => needle === "" || r.label.toLowerCase().includes(needle),
	);
	const shown = filtered.slice(0, PAGE * pages);
	const picked = new Set(selected);
	const onCount = filtered.filter((r) => picked.has(r.key)).length;
	const header =
		onCount === 0
			? false
			: onCount === filtered.length
				? true
				: "indeterminate";
	const rowId = (key: string) => `${id}-${key}`;

	useEffect(() => {
		if (focusKey === undefined) return;
		document.getElementById(rowId(focusKey))?.focus();
		setFocusKey(undefined);
	}, [focusKey]);

	const toggle = (key: string, on: boolean) =>
		onSelect(on ? [...selected, key] : selected.filter((k) => k !== key));
	const toggleAll = (on: boolean) => {
		const keys = new Set(filtered.map((r) => r.key));
		onSelect(
			on
				? [
						...selected,
						...filtered.filter((r) => !picked.has(r.key)).map((r) => r.key),
					]
				: selected.filter((k) => !keys.has(k)),
		);
	};

	return (
		<div className="flex flex-col gap-2">
			<InputGroup className="h-10">
				<InputGroupAddon>
					<Search aria-hidden />
				</InputGroupAddon>
				<InputGroupInput
					type="search"
					className="h-full"
					aria-label={text.search(rows.length, searchNoun ?? plural)}
					placeholder={text.search(rows.length, searchNoun ?? plural)}
					autoComplete="off"
					spellCheck={false}
					value={search}
					onChange={(e) => {
						setSearch(e.target.value);
						setPages(1);
					}}
				/>
			</InputGroup>
			{filtered.length === 0 ? (
				<p className="text-sm text-muted-foreground">
					{needle === ""
						? text.nothingFound(plural)
						: text.noMatch(search.trim(), plural)}
				</p>
			) : (
				<div className="@container overflow-hidden rounded-lg border bg-card">
					<Table>
						<TableHeader className="bg-muted">
							<TableRow>
								<TableHead className={cn(PAD, "w-8")}>
									<Checkbox
										aria-label={text.selectAll(plural)}
										checked={header}
										onCheckedChange={(on) => toggleAll(on === true)}
									/>
								</TableHead>
								<TableHead className={PAD}>{column}</TableHead>
								<TableHead className={cn(PAD, WIDE)}>
									{text.guests.hostColumn}
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{shown.map((row) => (
								<TableRow key={row.key} className="h-12">
									<TableCell className={PAD}>
										<Checkbox
											id={rowId(row.key)}
											checked={picked.has(row.key)}
											onCheckedChange={(on) => toggle(row.key, on === true)}
										/>
									</TableCell>
									<TableCell className={cn(PAD, "max-w-0 font-semibold")}>
										<label htmlFor={rowId(row.key)} className="block">
											<Truncated text={row.label} />
										</label>
										{row.hosts === undefined && (
											<span className="block text-sm font-normal text-muted-foreground @min-[440px]:hidden">
												{text.notFoundRow}
											</span>
										)}
									</TableCell>
									<TableCell
										className={cn(
											PAD,
											WIDE,
											"max-w-0 text-sm text-muted-foreground",
										)}
									>
										<Truncated
											text={row.hosts?.join(", ") ?? text.notFoundRow}
										/>
									</TableCell>
								</TableRow>
							))}
						</TableBody>
					</Table>
				</div>
			)}
			{shown.length < filtered.length && (
				<div className="flex flex-wrap items-center gap-2">
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="self-start"
						onClick={() => {
							setFocusKey(filtered[shown.length].key);
							setPages(pages + 1);
						}}
					>
						{text.showMore(PAGE)}
					</Button>
					<span className="text-sm text-muted-foreground">
						{text.showing(shown.length, filtered.length, plural)}
					</span>
				</div>
			)}
			<p role="status" className="text-sm text-muted-foreground">
				{excluding
					? text.excluded(selected.length, noun)
					: text.selected(selected.length, noun)}
			</p>
		</div>
	);
}

function PoolSelect({
	pools,
	value,
	onChange,
}: {
	pools: SourceOptions["pools"];
	value: string;
	onChange: (pool: string) => void;
}) {
	const lost = value !== "" && !pools.some((p) => p.name === value);
	if (pools.length === 0 && !lost) {
		return (
			<>
				<Select disabled>
					<SelectTrigger
						aria-label={text.guests.modes.pool}
						className="w-full data-[size=default]:h-10"
					>
						<SelectValue placeholder={text.guests.poolPlaceholder} />
					</SelectTrigger>
				</Select>
				<p className="text-sm text-muted-foreground">{text.guests.noPools}</p>
			</>
		);
	}
	return (
		<Select value={value} onValueChange={onChange}>
			<SelectTrigger
				aria-label={text.guests.modes.pool}
				className="w-full data-[size=default]:h-10"
			>
				<SelectValue placeholder={text.guests.poolPlaceholder} />
			</SelectTrigger>
			<SelectContent>
				{lost && (
					<SelectItem value={value}>
						{value}
						<span className="text-muted-foreground">{text.notFoundRow}</span>
					</SelectItem>
				)}
				{pools.map((pool) => (
					<SelectItem key={pool.name} value={pool.name}>
						{pool.name}
						<span className="text-muted-foreground">
							{guestCount(pool.count)}
						</span>
					</SelectItem>
				))}
			</SelectContent>
		</Select>
	);
}

function TagChips({
	tags,
	value,
	onChange,
}: {
	tags: SourceOptions["tags"];
	value: string[];
	onChange: (tags: string[]) => void;
}) {
	const chips = [
		...value
			.filter((v) => !tags.some((t) => t.name === v))
			.map((name) => ({ name, count: undefined as number | undefined })),
		...tags,
	];
	if (chips.length === 0) {
		return (
			<p className="text-sm text-muted-foreground">{text.guests.noTags}</p>
		);
	}
	const full = value.length >= MAX_TAGS;
	return (
		<>
			<ToggleGroup
				type="multiple"
				variant="outline"
				size="sm"
				spacing={2}
				aria-label={text.guests.modes.tags}
				className="flex-wrap"
				value={value}
				onValueChange={onChange}
			>
				{chips.map(({ name, count }) => (
					<ToggleGroupItem
						key={name}
						value={name}
						disabled={full && !value.includes(name)}
						className="gap-2 px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
					>
						{name}{" "}
						{count !== undefined && (
							<span className="font-normal text-muted-foreground tabular-nums group-data-[state=on]/toggle:text-primary-strong">
								{count}
							</span>
						)}
					</ToggleGroupItem>
				))}
			</ToggleGroup>
			{full && (
				<p className="text-sm text-muted-foreground">{text.guests.tagLimit}</p>
			)}
		</>
	);
}

function PickerNotes({ data, noun }: { data: SourceOptions; noun: Noun }) {
	const plural = text.plural[noun];
	const hint =
		noun === "guest" ? text.guests.truncatedHint : text.databases.truncatedHint;
	return (
		<>
			{data.truncated_hosts.map((host) => (
				<p key={host.hostname} className="text-sm text-muted-foreground">
					{text.truncated(host.hostname, host.count, plural, hint)}
				</p>
			))}
			{data.unreachable_hosts.map((host) => (
				<p key={host} className="flex items-start gap-1 text-sm text-warning">
					<TriangleAlert aria-hidden className="mt-1 size-4 shrink-0" />
					{text.unreachable(host, plural)}
				</p>
			))}
		</>
	);
}
