import { keepPreviousData, useInfiniteQuery } from "@tanstack/react-query";
import { File, FileQuestion, Folder, Link as LinkIcon } from "lucide-react";
import { memo, useCallback, useRef } from "react";
import {
	getListSnapshotEntriesQueryKey,
	listSnapshotEntries,
} from "@/api/gen/endpoints";
import type { EntryItem, EntryPage } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import {
	coveringFolder,
	type SelectedItem,
	type Selection,
	selectedInside,
} from "@/components/restore/use-selection";
import { Checkbox } from "@/components/ui/checkbox";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import {
	formatBytes,
	formatDateTime,
	formatDateTimeSeconds,
	formatMode,
} from "@/lib/format";
import { absolutePath, decodeB64, displayName, pathParts } from "@/lib/names";

export const PAGE_SIZE = 200;
/** Keeps at most 1,000 rows in the DOM; older pages drop off as new ones load. */
const MAX_PAGES = 5;
const SKELETON_ROWS = 5;
const SHOW_MODIFIED = "hidden @min-[640px]:table-cell";
const SHOW_DETAILS = "hidden @min-[880px]:table-cell";
const SHOW_INLINE_SIZE = "@min-[640px]:hidden";

export interface EntryRow {
	entry: EntryItem;
	/** Parts of the folder that holds the entry. */
	dir: Uint8Array[];
	pathB64: string;
	display: string;
	parent?: { display: string; pathB64: string };
}

export const itemOf = (row: EntryRow): SelectedItem => ({
	pathB64: row.pathB64,
	display: row.display,
	isDir: row.entry.kind === "dir",
});

export function useFolderEntries(snapshot: string, path: string) {
	return useInfiniteQuery({
		queryKey: [
			...getListSnapshotEntriesQueryKey(snapshot, path ? { path } : undefined),
			"pages",
		],
		initialPageParam: 0,
		queryFn: ({ pageParam, signal }) =>
			listSnapshotEntries(
				snapshot,
				{ path: path || undefined, offset: pageParam },
				undefined,
				signal,
			),
		getNextPageParam: (last) => {
			const end = last.offset + last.entries.length;
			return end < last.total ? end : undefined;
		},
		getPreviousPageParam: (first) =>
			first.offset > 0 ? Math.max(0, first.offset - PAGE_SIZE) : undefined,
		maxPages: MAX_PAGES,
		placeholderData: keepPreviousData,
	});
}

export function rowsOf(pages: EntryPage[]): EntryRow[] {
	const first = pages[0];
	if (!first) return [];
	const dir = pathParts(first.path_b64);
	return pages.flatMap((page) =>
		page.entries.map((entry) => ({
			entry,
			dir,
			pathB64: absolutePath([...dir, decodeB64(entry.name_b64)]),
			display: displayName(`${first.path}/${entry.name}`),
		})),
	);
}

function EntryIcon({ kind }: { kind: EntryItem["kind"] }) {
	const props = {
		"aria-hidden": true,
		className: "size-4 shrink-0 text-muted-foreground",
	} as const;
	if (kind === "dir") return <Folder {...props} />;
	if (kind === "symlink") return <LinkIcon {...props} />;
	if (kind === "other") return <FileQuestion {...props} />;
	return <File {...props} />;
}

// ponytail: length heuristic instead of measuring overflow; a name that fits needs no tooltip.
const TOOLTIP_MIN = 24;

function Truncated({
	text,
	children,
}: {
	text: string;
	children: React.ReactElement;
}) {
	if (text.length <= TOOLTIP_MIN) return children;
	return (
		<Tooltip>
			<TooltipTrigger asChild>{children}</TooltipTrigger>
			<TooltipContent className="max-w-lg wrap-anywhere">
				<TextWithPaths text={text} />
			</TooltipContent>
		</Tooltip>
	);
}

function EntryName({
	row,
	onOpen,
}: {
	row: EntryRow;
	onOpen: (pathB64: string) => void;
}) {
	const { entry } = row;
	const name = displayName(entry.name);
	return (
		<div className="flex min-w-0 items-center gap-1">
			<EntryIcon kind={entry.kind} />
			{entry.kind === "dir" ? (
				<Truncated text={name}>
					<button
						type="button"
						aria-label={copy.restore.browser.openFolder(name)}
						className="min-w-0 truncate rounded-sm text-left font-mono text-sm text-primary-strong hover:underline"
						onClick={() => onOpen(row.pathB64)}
					>
						{name}
					</button>
				</Truncated>
			) : (
				<Truncated text={name}>
					<span className="min-w-0 truncate font-mono text-sm">{name}</span>
				</Truncated>
			)}
			{entry.kind === "symlink" && entry.link_target != null && (
				<Truncated text={displayName(entry.link_target)}>
					<span className="min-w-0 flex-1 truncate font-mono text-sm text-muted-foreground">
						{copy.restore.browser.linkTarget(displayName(entry.link_target))}
					</span>
				</Truncated>
			)}
		</div>
	);
}

function SkeletonRows() {
	return Array.from({ length: SKELETON_ROWS }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-12">
			<TableCell className="w-10 pl-4" />
			<TableCell className="w-full max-w-0 px-4">
				<Skeleton className="h-4 w-48 max-w-full" />
			</TableCell>
			<TableCell className={`px-4 ${SHOW_MODIFIED}`}>
				<Skeleton className="h-4 w-24" />
			</TableCell>
			<TableCell className={`px-4 ${SHOW_MODIFIED}`}>
				<Skeleton className="ml-auto h-4 w-14" />
			</TableCell>
			<TableCell className={`px-4 ${SHOW_DETAILS}`}>
				<Skeleton className="h-4 w-20" />
			</TableCell>
			<TableCell className={`px-4 ${SHOW_DETAILS}`}>
				<Skeleton className="h-4 w-14" />
			</TableCell>
		</TableRow>
	));
}

const EntryRowView = memo(function EntryRowView({
	row,
	selected,
	includedWith,
	onToggle,
	onOpen,
}: {
	row: EntryRow;
	selected: boolean;
	includedWith?: string;
	onToggle: (row: EntryRow, range: boolean) => void;
	onOpen: (pathB64: string) => void;
}) {
	const { entry } = row;
	return (
		<TableRow
			className="h-12 data-[state=selected]:bg-primary-subtle"
			data-state={selected ? "selected" : undefined}
		>
			<TableCell className="w-10 pl-4">
				<Checkbox
					aria-label={copy.restore.selection.select(displayName(entry.name))}
					checked={selected}
					disabled={includedWith !== undefined}
					title={
						includedWith && copy.restore.selection.includedWith(includedWith)
					}
					onClick={(event) => {
						if (!event.shiftKey) return;
						event.preventDefault();
						onToggle(row, true);
					}}
					onCheckedChange={() => onToggle(row, false)}
				/>
			</TableCell>
			<TableCell className="w-full max-w-0 px-4">
				<EntryName row={row} onOpen={onOpen} />
				{row.parent && (
					<Truncated text={row.parent.display}>
						<button
							type="button"
							aria-label={copy.restore.browser.search.openParent(
								row.parent.display,
							)}
							dir="rtl"
							className="block max-w-full truncate rounded-sm text-left font-mono text-sm text-muted-foreground hover:underline"
							onClick={() => onOpen(row.parent?.pathB64 ?? "")}
						>
							<span dir="ltr">{row.parent.display}</span>
						</button>
					</Truncated>
				)}
				{entry.kind === "file" && (
					<span
						className={`block text-sm text-muted-foreground tabular-nums ${SHOW_INLINE_SIZE}`}
					>
						{formatBytes(entry.size)}
					</span>
				)}
			</TableCell>
			<TableCell
				className={`px-4 text-sm ${SHOW_MODIFIED}`}
				title={
					entry.mtime_ms !== 0
						? formatDateTimeSeconds(entry.mtime_ms)
						: undefined
				}
			>
				{entry.mtime_ms !== 0
					? formatDateTime(entry.mtime_ms)
					: copy.common.dash}
			</TableCell>
			<TableCell
				className={`px-4 text-right text-sm tabular-nums ${SHOW_MODIFIED}`}
			>
				{entry.kind === "file" ? formatBytes(entry.size) : copy.common.dash}
			</TableCell>
			<TableCell className={`px-4 font-mono text-sm ${SHOW_DETAILS}`}>
				{formatMode(entry.mode)}
			</TableCell>
			<TableCell className={`px-4 font-mono text-sm ${SHOW_DETAILS}`}>
				{entry.uid}:{entry.gid}
			</TableCell>
		</TableRow>
	);
});

export function EntriesTable({
	rows,
	folder,
	selection,
	complete,
	onOpen,
}: {
	complete?: boolean;
	rows: EntryRow[] | undefined;
	folder?: SelectedItem;
	selection: Selection;
	onOpen: (pathB64: string) => void;
}) {
	const covers = new Map<Uint8Array[], SelectedItem | undefined>();
	const coveredBy = (dir: Uint8Array[]) => {
		if (!covers.has(dir)) covers.set(dir, coveringFolder(selection.items, dir));
		return covers.get(dir);
	};
	const folderParts = folder && pathParts(folder.pathB64);
	const folderCover =
		folderParts && coveringFolder(selection.items, folderParts);
	const folderPartly =
		folderCover === undefined &&
		folderParts !== undefined &&
		selectedInside(selection.items, folderParts);
	const everyRow =
		folderCover === undefined &&
		complete === true &&
		rows !== undefined &&
		rows.length > 0 &&
		rows.every((row) => selection.items.has(itemOf(row).pathB64));
	const latest = useRef(rows);
	latest.current = rows;
	const { toggle, toggleRange } = selection;
	const onToggle = useCallback(
		(row: EntryRow, range: boolean) => {
			if (range) toggleRange((latest.current ?? []).map(itemOf), itemOf(row));
			else toggle(itemOf(row));
		},
		[toggle, toggleRange],
	);
	return (
		<Table>
			<TableHeader className="bg-muted">
				<TableRow>
					<TableHead className="w-10 pl-4">
						{folder && (
							<Checkbox
								aria-label={copy.restore.selection.selectAll}
								checked={
									folderCover !== undefined ||
									everyRow ||
									(folderPartly && "indeterminate")
								}
								disabled={
									folderCover !== undefined &&
									folderCover.pathB64 !== folder.pathB64
								}
								title={
									folderCover && folderCover.pathB64 !== folder.pathB64
										? copy.restore.selection.includedWith(folderCover.display)
										: undefined
								}
								onCheckedChange={() => {
									if (everyRow) {
										for (const row of rows ?? [])
											selection.selectFolder(itemOf(row));
									} else {
										selection.selectFolder(folder);
									}
								}}
							/>
						)}
					</TableHead>
					<TableHead className="px-4">
						{copy.restore.browser.columns.name}
					</TableHead>
					<TableHead className={`px-4 ${SHOW_MODIFIED}`}>
						{copy.restore.browser.columns.modified}
					</TableHead>
					<TableHead className={`px-4 text-right ${SHOW_MODIFIED}`}>
						{copy.restore.browser.columns.size}
					</TableHead>
					<TableHead className={`px-4 ${SHOW_DETAILS}`}>
						{copy.restore.browser.columns.permissions}
					</TableHead>
					<TableHead className={`px-4 ${SHOW_DETAILS}`}>
						{copy.restore.browser.columns.owner}
					</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{rows === undefined ? (
					<SkeletonRows />
				) : (
					rows.map((row) => {
						const cover = coveredBy(row.dir);
						return (
							<EntryRowView
								key={row.pathB64}
								row={row}
								selected={
									cover !== undefined || selection.items.has(row.pathB64)
								}
								includedWith={cover?.display}
								onToggle={onToggle}
								onOpen={onOpen}
							/>
						);
					})
				)}
			</TableBody>
		</Table>
	);
}
