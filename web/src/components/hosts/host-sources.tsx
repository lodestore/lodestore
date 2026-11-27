import { keepPreviousData } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { CircleX, Info, TriangleAlert } from "lucide-react";
import { Fragment, type ReactNode, useState } from "react";
import { useHostSources } from "@/api/gen/endpoints";
import type {
	FilesSummary,
	HostDetail,
	HostSources as HostSourcesData,
	SourceGroup,
	SourceItem,
} from "@/api/gen/model";
import { formatRelative, RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Truncated } from "@/components/truncated";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
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
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import {
	SOURCE_TYPES,
	type SourceType,
	sourceTypeIcon,
	sourceTypeLabel,
	sourceTypeNoun,
} from "@/lib/source-type";
import { cn } from "@/lib/utils";

const route = getRouteApi("/_app/hosts/$id");

const REFRESH_MS = 15_000;
const PAGE_SIZE = 25;
const MAX_CLONES = 3;
const FROM_680 = "hidden @min-[680px]:table-cell";
const PAD = "px-2 @min-[680px]:px-4";
const NESTED = "pl-8 @min-[680px]:pl-8";
const SOURCE_WIDTH = "max-w-32 @min-[680px]:max-w-40 @min-[860px]:max-w-72";
const UNPROTECTED = "unprotected";
const ALL = "all";
const text = copy.sources.host;

type Show = typeof ALL | typeof UNPROTECTED;

type Entry =
	| { kind: "item"; item: SourceItem }
	| { kind: "files"; files: FilesSummary };

function TypeIcon({ type }: { type: string }) {
	const Icon = sourceTypeIcon[type as SourceType] ?? sourceTypeIcon.files;
	return <Icon aria-hidden className="size-4 shrink-0 text-muted-foreground" />;
}

function SourceCell({
	type,
	label,
	className = SOURCE_WIDTH,
}: {
	type: string;
	label: string;
	className?: string;
}) {
	return (
		<div className={cn("flex items-center gap-2", className)}>
			<TypeIcon type={type} />
			<Truncated text={label} className="min-w-0" />
		</div>
	);
}

function PolicyLinks({ policies }: { policies: string[] }) {
	const sorted = [...policies].sort((a, b) => a.localeCompare(b));
	const [first, ...rest] = sorted;
	return (
		<span className="flex items-center gap-1">
			<Link
				to="/policies/$name"
				params={{ name: first }}
				className="font-semibold text-primary-strong underline-offset-4 hover:underline"
			>
				{first}
			</Link>
			{rest.length > 0 && (
				<Tooltip>
					<TooltipTrigger asChild>
						<span className="text-muted-foreground">
							{text.more(rest.length)}
						</span>
					</TooltipTrigger>
					<TooltipContent>{sorted.join(", ")}</TooltipContent>
				</Tooltip>
			)}
		</span>
	);
}

function Protection({
	protection,
	alertOpen,
}: Pick<SourceItem, "protection"> & { alertOpen: boolean }) {
	if (protection.state === "agent_too_old") {
		return (
			<Badge variant="outline" className="gap-1 text-neutral">
				<Info aria-hidden />
				{text.protection.agentTooOld}
			</Badge>
		);
	}
	if (protection.state === "protected" && protection.policies.length > 0) {
		return <PolicyLinks policies={protection.policies} />;
	}
	return (
		<span className="flex flex-wrap items-center gap-x-2 gap-y-1">
			<StatusBadge status="warning">{text.protection.notProtected}</StatusBadge>
			{alertOpen && (
				<span className="text-sm text-muted-foreground">
					{text.protection.alertOpen}
				</span>
			)}
		</span>
	);
}

function LastSnapshot({ at }: { at: number | null | undefined }) {
	return at == null ? (
		<span className="text-muted-foreground">{text.never}</span>
	) : (
		<RelativeTime value={new Date(at)} tabIndex={-1} />
	);
}

const guestDetails = (item: SourceItem) => {
	const parts = [item.pool, ...item.tags].filter(Boolean);
	return parts.length > 0 ? parts.join(" · ") : copy.common.dash;
};

const groupLabel = (group: SourceGroup) =>
	text.group(
		sourceTypeLabel(group.source_type),
		group.version || undefined,
		group.port,
	);

const joinNouns = (nouns: string[]) =>
	nouns.length > 1
		? `${nouns.slice(0, -1).join(", ")} and ${nouns[nouns.length - 1]}`
		: nouns.join("");

function ItemRow({ item }: { item: SourceItem }) {
	const nested = item.group_id != null;
	return (
		<TableRow className="h-12">
			<TableCell className={cn(PAD, "text-sm font-semibold", nested && NESTED)}>
				{nested ? (
					<Truncated text={item.name ?? item.label} className={SOURCE_WIDTH} />
				) : (
					<SourceCell type={item.source_type} label={item.label} />
				)}
			</TableCell>
			<TableCell className={cn(PAD, FROM_680, "text-sm text-muted-foreground")}>
				{nested
					? item.oid != null
						? text.oid(item.oid)
						: copy.common.dash
					: guestDetails(item)}
			</TableCell>
			<TableCell className={cn(PAD, "text-sm")}>
				<Protection protection={item.protection} alertOpen={item.alert_open} />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680, "text-sm")}>
				<LastSnapshot at={item.last_snapshot_at} />
			</TableCell>
		</TableRow>
	);
}

function FilesRow({ files }: { files: FilesSummary }) {
	return (
		<TableRow className="h-12">
			<TableCell className={cn(PAD, "text-sm font-semibold")}>
				<SourceCell type="files" label={text.filesLabel} />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680, "text-sm text-muted-foreground")}>
				<Truncated text={files.paths.join(", ")} className="max-w-56" />
			</TableCell>
			<TableCell className={cn(PAD, "text-sm")}>
				<PolicyLinks policies={files.policies} />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680, "text-sm")}>
				<LastSnapshot at={files.last_snapshot_at} />
			</TableCell>
		</TableRow>
	);
}

function GroupRow({ group }: { group: SourceGroup }) {
	return (
		<TableRow className="h-12 bg-muted/50">
			<TableCell colSpan={4} className={cn(PAD, "text-sm font-semibold")}>
				<SourceCell
					type={group.source_type}
					label={groupLabel(group)}
					className="min-w-0"
				/>
			</TableCell>
		</TableRow>
	);
}

function UnreachableRow({ group }: { group: SourceGroup }) {
	return (
		<TableRow className="min-h-12">
			<TableCell
				colSpan={4}
				className={cn(PAD, NESTED, "text-sm whitespace-normal")}
			>
				<span className="flex items-start gap-1 text-destructive">
					<CircleX aria-hidden className="mt-1 size-4 shrink-0" />
					{group.source_type === "mysql"
						? text.unreachable.mysql
						: text.unreachable.postgres}
				</span>
			</TableCell>
		</TableRow>
	);
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-12">
			<TableCell className={PAD}>
				<Skeleton className="h-4 w-32" />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680)}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={PAD}>
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell className={cn(PAD, FROM_680)}>
				<Skeleton className="h-4 w-16" />
			</TableCell>
		</TableRow>
	));
}

function SourcesToolbar({
	data,
	counts,
	show,
	onChange,
}: {
	data: HostSourcesData;
	counts: { all: number; unprotected: number };
	show: Show;
	onChange: (next: Show) => void;
}) {
	const when =
		data.collected_at == null
			? undefined
			: formatRelative(new Date(data.collected_at), new Date());
	const scope = data.scope
		? text.scope[data.scope.kind](data.scope.name)
		: undefined;
	const found =
		when === undefined
			? undefined
			: scope
				? text.foundWithScope(scope, when)
				: text.found(when);
	const options: { value: Show; label: string; count: number }[] = [
		{ value: ALL, label: text.filter.all, count: counts.all },
		{
			value: UNPROTECTED,
			label: text.filter.unprotected,
			count: counts.unprotected,
		},
	];
	const current = options.find((o) => o.value === show) ?? options[0];
	const select = text.filter.selectValue(current.label, current.count);
	const change = (next: string) =>
		onChange(next === UNPROTECTED ? UNPROTECTED : ALL);

	return (
		<div className="@container">
			<div className="flex flex-col gap-2 @min-[680px]:flex-row @min-[680px]:flex-wrap @min-[680px]:items-center @min-[680px]:gap-4">
				{counts.unprotected > 0 && (
					<>
						<ToggleGroup
							type="single"
							variant="outline"
							size="lg"
							spacing={0}
							aria-label={text.filter.label}
							className="hidden @min-[680px]:flex"
							value={show}
							onValueChange={(next) => next && change(next)}
						>
							{options.map(({ value, label, count }) => (
								<ToggleGroupItem
									key={value}
									value={value}
									className="gap-2 px-4 font-normal data-[state=on]:bg-primary-subtle data-[state=on]:font-semibold data-[state=on]:text-primary-strong"
								>
									{label}
									<span className="font-normal text-muted-foreground tabular-nums group-data-[state=on]/toggle:font-semibold group-data-[state=on]/toggle:text-primary-strong">
										{count}
									</span>
								</ToggleGroupItem>
							))}
						</ToggleGroup>
						<Select value={show} onValueChange={change}>
							<SelectTrigger
								aria-label={select}
								className="w-full data-[size=default]:h-10 @min-[680px]:hidden"
							>
								<SelectValue>{select}</SelectValue>
							</SelectTrigger>
							<SelectContent>
								{options.map(({ value, label, count }) => (
									<SelectItem key={value} value={value}>
										{label}
										<span className="text-muted-foreground tabular-nums">
											{count}
										</span>
									</SelectItem>
								))}
							</SelectContent>
						</Select>
					</>
				)}
				{found && (
					<p className="text-sm text-muted-foreground @min-[680px]:ml-auto">
						{found}
					</p>
				)}
			</div>
		</div>
	);
}

function Note({
	icon,
	className,
	children,
}: {
	icon?: ReactNode;
	className: string;
	children: ReactNode;
}) {
	return (
		<p className={cn("flex items-start gap-1 text-sm", className)}>
			{icon}
			{children}
		</p>
	);
}

function SourceNotes({ data }: { data: HostSourcesData }) {
	const tooOld = SOURCE_TYPES.filter((type) =>
		data.items.some(
			(i) => i.source_type === type && i.protection.state === "agent_too_old",
		),
	).flatMap((type) => sourceTypeNoun(type) ?? []);
	const clones = [
		...data.items
			.filter((i) => i.group_id == null && i.shared_with.length > 0)
			.map((i) => text.clone.source(i.label, i.shared_with.join(", "))),
		...data.groups
			.filter((g) => g.shared_with.length > 0)
			.map((g) => text.clone.cluster(groupLabel(g), g.shared_with.join(", "))),
	];
	if (!data.truncated && tooOld.length === 0 && clones.length === 0) {
		return null;
	}
	return (
		<div className="flex flex-col gap-2">
			{data.truncated && (
				<Note className="text-muted-foreground">
					{text.truncated(data.items.length)}
				</Note>
			)}
			{tooOld.length > 0 && (
				<Note className="text-muted-foreground">
					{text.agentTooOldNote(joinNouns(tooOld))}
				</Note>
			)}
			{clones.slice(0, MAX_CLONES).map((line) => (
				<Note
					key={line}
					className="text-warning"
					icon={<TriangleAlert aria-hidden className="mt-1 size-4 shrink-0" />}
				>
					{line}
				</Note>
			))}
			{clones.length > MAX_CLONES && (
				<Note className="text-muted-foreground">
					{text.clone.more(clones.length - MAX_CLONES)}
				</Note>
			)}
		</div>
	);
}

function pageRows(entries: Entry[], groups: Map<string, SourceGroup>) {
	const rows: ReactNode[] = [];
	let current: string | undefined;
	entries.forEach((entry, i) => {
		if (entry.kind === "files") {
			rows.push(<FilesRow key="files" files={entry.files} />);
			return;
		}
		const id = entry.item.group_id ?? undefined;
		const group = id === undefined ? undefined : groups.get(id);
		if (group && id !== current) {
			rows.push(<GroupRow key={`group-${id}-${i}`} group={group} />);
		}
		current = id;
		rows.push(<ItemRow key={`item-${i}`} item={entry.item} />);
	});
	return rows;
}

export function HostSources({ host }: { host: HostDetail }) {
	const [page, setPage] = useState(1);
	const requested = route.useSearch().show;
	const navigate = route.useNavigate();
	const query = useHostSources(host.id, {
		query: {
			placeholderData: keepPreviousData,
			refetchInterval: REFRESH_MS,
			refetchIntervalInBackground: false,
		},
	});
	const data = query.data;

	if (data === undefined && query.isError) {
		return (
			<Alert variant="destructive" role="alert" className="mt-4">
				<AlertDescription className="flex items-center justify-between gap-4">
					{text.loadFailed}
					<Button
						type="button"
						variant="outline"
						size="sm"
						onClick={() => query.refetch()}
					>
						{text.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}

	if (
		data !== undefined &&
		data.items.length === 0 &&
		data.groups.length === 0 &&
		!data.files
	) {
		return (
			<Empty className="py-24">
				<EmptyHeader>
					<EmptyTitle>{text.empty.heading}</EmptyTitle>
					<EmptyDescription>{text.empty.body}</EmptyDescription>
				</EmptyHeader>
			</Empty>
		);
	}

	const files =
		data?.files && data.files.policies.length > 0 ? data.files : undefined;
	const entries: Entry[] = [
		...(data?.items ?? []).map((item): Entry => ({ kind: "item", item })),
		...(files ? [{ kind: "files", files } as Entry] : []),
	];
	const unprotected = entries.filter(
		(e) => e.kind === "item" && e.item.protection.state === UNPROTECTED,
	);
	const show: Show =
		requested === UNPROTECTED && unprotected.length > 0 ? UNPROTECTED : ALL;
	const shown = show === UNPROTECTED ? unprotected : entries;
	const lastPage = Math.max(1, Math.ceil(shown.length / PAGE_SIZE));
	const current = Math.min(page, lastPage);
	const from = (current - 1) * PAGE_SIZE;
	const slice = shown.slice(from, from + PAGE_SIZE);
	const groups = new Map((data?.groups ?? []).map((g) => [g.id, g]));
	const unreachable = (data?.groups ?? []).filter((g) => g.unreachable);
	const databases =
		(data?.groups.length ?? 0) > 0 ||
		entries.some((e) => e.kind === "item" && e.item.group_id != null);

	function setShow(next: Show) {
		setPage(1);
		void navigate({
			search: (s) => ({ ...s, show: next === UNPROTECTED ? next : undefined }),
			replace: true,
		});
	}

	return (
		<div className="flex flex-col gap-4 pt-4">
			{data !== undefined && (
				<SourcesToolbar
					data={data}
					counts={{ all: entries.length, unprotected: unprotected.length }}
					show={show}
					onChange={setShow}
				/>
			)}
			<div className="@container overflow-hidden rounded-lg border bg-card">
				<Table>
					<TableHeader className="bg-muted">
						<TableRow>
							<TableHead className={PAD}>{text.columns.source}</TableHead>
							<TableHead className={cn(PAD, FROM_680)}>
								{databases ? text.columns.details : text.columns.poolTags}
							</TableHead>
							<TableHead className={PAD}>{text.columns.protection}</TableHead>
							<TableHead className={cn(PAD, FROM_680)}>
								{text.columns.lastSnapshot}
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{data === undefined ? (
							<SkeletonRows />
						) : (
							<>
								{pageRows(slice, groups)}
								{unreachable.map((group) => (
									<Fragment key={group.id}>
										<GroupRow group={group} />
										<UnreachableRow group={group} />
									</Fragment>
								))}
							</>
						)}
					</TableBody>
				</Table>
			</div>
			{data !== undefined && shown.length > PAGE_SIZE && (
				<div className="flex flex-wrap items-center justify-between gap-4">
					<p className="text-sm text-muted-foreground">
						{text.showing(from + 1, from + slice.length, shown.length)}
					</p>
					<div className="flex gap-2">
						<Button
							variant="outline"
							size="sm"
							disabled={current <= 1}
							onClick={() => setPage(current - 1)}
						>
							{copy.jobs.previous}
						</Button>
						<Button
							variant="outline"
							size="sm"
							disabled={current >= lastPage}
							onClick={() => setPage(current + 1)}
						>
							{copy.jobs.next}
						</Button>
					</div>
				</div>
			)}
			{data !== undefined && <SourceNotes data={data} />}
		</div>
	);
}
