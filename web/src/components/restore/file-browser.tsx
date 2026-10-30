import { getRouteApi } from "@tanstack/react-router";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useGetSnapshot, useListRestoreHosts } from "@/api/gen/endpoints";
import { DownloadDialog } from "@/components/restore/download-dialog";
import {
	EntriesTable,
	PAGE_SIZE,
	rowsOf,
	useFolderEntries,
} from "@/components/restore/entries-table";
import { isRestoreTarget } from "@/components/restore/host-picker";
import { PathBar } from "@/components/restore/path-bar";
import { RestoreDialog } from "@/components/restore/restore-dialog";
import {
	FilterField,
	SearchResults,
} from "@/components/restore/search-results";
import {
	SelectionActions,
	SelectionBar,
} from "@/components/restore/selection-bar";
import type {
	SelectedItem,
	Selection,
} from "@/components/restore/use-selection";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatCount } from "@/lib/format";
import {
	absolutePath,
	decodeB64,
	decodeText,
	displayName,
	pathParts,
} from "@/lib/names";

const route = getRouteApi("/_app/restore");

const shownRange = (offset: number, count: number) =>
	offset > 0
		? `${formatCount(offset + 1)}–${formatCount(offset + count)}`
		: formatCount(count);

export function FileBrowser({
	snapshot,
	path,
	q,
	selection,
	preselect,
}: {
	snapshot: string;
	path: string;
	q?: string;
	selection: Selection;
	preselect?: string;
}) {
	const navigate = route.useNavigate();
	const heading = useRef<HTMLHeadingElement>(null);
	const counter = useRef<HTMLParagraphElement>(null);
	const focusNext = useRef(false);
	const listing = useFolderEntries(snapshot, path);
	const pages = listing.data?.pages;
	const first = pages?.[0];
	const rows = useMemo(() => (pages ? rowsOf(pages) : undefined), [pages]);
	const total = pages?.at(-1)?.total ?? 0;
	const settled = listing.isSuccess && !listing.isPlaceholderData;

	const detail = useGetSnapshot(snapshot);
	const canRestore = (useListRestoreHosts().data ?? []).some(isRestoreTarget);
	const [dialog, setDialog] = useState<"restore" | "download">();
	const restoreButton = useRef<HTMLButtonElement>(null);
	const downloadButton = useRef<HTMLButtonElement>(null);
	const items = useMemo(
		() => Array.from(selection.items.keys()),
		[selection.items],
	);
	const returnFocus =
		(target: React.RefObject<HTMLButtonElement | null>) => (event: Event) => {
			event.preventDefault();
			target.current?.focus();
		};

	const preselected = useRef(false);
	const selectItem = selection.selectFolder;
	useEffect(() => {
		if (!preselect || preselected.current) return;
		preselected.current = true;
		selectItem({
			pathB64: absolutePath(pathParts(preselect)),
			display: displayName(decodeText(decodeB64(preselect))),
			isDir: false,
		});
	}, [preselect, selectItem]);

	const [noMatch, setNoMatch] = useState(false);
	const [opened, setOpened] = useState(0);
	useEffect(() => {
		if (!settled || !focusNext.current || opened === 0) return;
		focusNext.current = false;
		heading.current?.focus({ preventScroll: true });
	}, [settled, opened]);

	const open = useCallback(
		(target: string) => {
			focusNext.current = true;
			void navigate({
				search: (prev) => ({
					...prev,
					path: target || undefined,
					q: undefined,
				}),
			}).then(() => setOpened((n) => n + 1));
		},
		[navigate],
	);

	const filter = (next: string | undefined) =>
		void navigate({ search: (prev) => ({ ...prev, q: next }), replace: true });

	const shownPath = displayName(first ? first.path || "/" : "/");
	const folder: SelectedItem = {
		pathB64: absolutePath(pathParts(first?.path_b64 ?? path)),
		display: shownPath,
		isDir: true,
	};
	return (
		<div className="@container flex min-w-0 flex-col overflow-clip rounded-lg border bg-card">
			<div className="flex min-h-12 flex-wrap items-center justify-between gap-2 border-b px-4 py-2">
				{q ? (
					<div className="flex min-w-0 items-center gap-2">
						<p className="min-w-0 truncate">
							{copy.restore.browser.search.results(displayName(q), shownPath)}
						</p>
						<Button
							type="button"
							variant="outline"
							size="sm"
							onClick={() => filter(undefined)}
						>
							{copy.restore.browser.search.clear}
						</Button>
					</div>
				) : (
					<div className="min-w-0 flex-1">
						<PathBar pathB64={first?.path_b64 ?? path} onOpen={open} />
					</div>
				)}
				<FilterField q={q} onChange={filter} />
			</div>
			<h2 ref={heading} tabIndex={-1} className="sr-only">
				{copy.restore.browser.contents(shownPath)}
			</h2>
			{q ? (
				<SearchResults
					key={`${path}|${q}`}
					snapshot={snapshot}
					path={path}
					q={q}
					pathLabel={shownPath}
					selection={selection}
					onOpen={open}
					onNoMatch={setNoMatch}
				/>
			) : listing.isError ? (
				<Alert variant="destructive" role="alert" className="m-4 w-auto">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.restore.browser.listFailed(listing.error.message)}
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => listing.refetch()}
						>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			) : (
				<>
					{listing.hasPreviousPage && (
						<div className="border-b px-4 py-2">
							<Button
								type="button"
								variant="outline"
								size="sm"
								disabled={listing.isFetchingPreviousPage}
								onClick={() =>
									listing.fetchPreviousPage().then((result) => {
										if (!result.hasPreviousPage) heading.current?.focus();
									})
								}
							>
								{listing.isFetchingPreviousPage && <Spinner />}
								{copy.restore.browser.previous}
							</Button>
						</div>
					)}
					<EntriesTable
						rows={rows}
						folder={folder}
						selection={selection}
						complete={rows !== undefined && rows.length === total}
						onOpen={open}
					/>
					{rows?.length === 0 && (
						<p className="px-4 py-8 text-sm text-muted-foreground">
							{copy.restore.browser.empty}
						</p>
					)}
					{rows !== undefined && total > PAGE_SIZE && (
						<div className="flex items-center justify-between gap-4 border-t px-4 py-2">
							<p
								ref={counter}
								tabIndex={-1}
								className="text-sm text-muted-foreground tabular-nums"
							>
								{copy.restore.browser.showing(
									shownRange(first?.offset ?? 0, rows.length),
									formatCount(total),
								)}
							</p>
							{listing.hasNextPage && (
								<Button
									type="button"
									variant="outline"
									size="sm"
									disabled={listing.isFetchingNextPage}
									onClick={() =>
										listing.fetchNextPage().then((result) => {
											if (!result.hasNextPage) counter.current?.focus();
										})
									}
								>
									{listing.isFetchingNextPage && <Spinner />}
									{copy.restore.browser.next}
								</Button>
							)}
						</div>
					)}
				</>
			)}
			<SelectionBar
				selection={selection}
				hint={!(q ? noMatch : rows?.length === 0)}
				actions={
					<SelectionActions
						ready={detail.data !== undefined}
						canRestore={canRestore}
						restoreRef={restoreButton}
						onRestore={() => setDialog("restore")}
						downloadRef={downloadButton}
						onDownload={() => setDialog("download")}
					/>
				}
			/>
			<RestoreDialog
				open={dialog === "restore"}
				onOpenChange={(open) => !open && setDialog(undefined)}
				snapshot={detail.data}
				items={items}
				onCloseAutoFocus={returnFocus(restoreButton)}
			/>
			<DownloadDialog
				open={dialog === "download"}
				onOpenChange={(open) => !open && setDialog(undefined)}
				snapshot={detail.data}
				items={items}
				onCloseAutoFocus={returnFocus(downloadButton)}
			/>
		</div>
	);
}
