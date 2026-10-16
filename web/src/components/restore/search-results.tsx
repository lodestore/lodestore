import { Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { SearchLine, SearchMatch } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import {
	EntriesTable,
	type EntryRow,
} from "@/components/restore/entries-table";
import type { Selection } from "@/components/restore/use-selection";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { absolutePath, decodeB64, displayName, pathParts } from "@/lib/names";
import { streamNdjson } from "@/lib/ndjson";
import { matchCount } from "@/lib/plural";

interface State {
	rows: EntryRow[];
	status: "searching" | "done" | "error";
	truncated: boolean;
	count: number;
	error?: string;
}

const SEARCHING: State = {
	rows: [],
	status: "searching",
	truncated: false,
	count: 0,
};

const DEBOUNCE_MS = 400;

export function FilterField({
	q,
	onChange,
}: {
	q: string | undefined;
	onChange: (q: string | undefined) => void;
}) {
	const text = copy.restore.browser.filter;
	const [input, setInput] = useState(q ?? "");
	const sent = useRef(q);
	const emit = useRef(onChange);
	emit.current = onChange;

	useEffect(() => {
		if (q === sent.current) return;
		sent.current = q;
		setInput(q ?? "");
	}, [q]);

	useEffect(() => {
		const next = input || undefined;
		if (next === sent.current) return;
		const timer = setTimeout(() => {
			sent.current = next;
			emit.current(next);
		}, DEBOUNCE_MS);
		return () => clearTimeout(timer);
	}, [input]);

	return (
		<div className="relative w-full @min-[880px]:w-90">
			<Search
				aria-hidden
				className="pointer-events-none absolute top-3 left-3 size-4 text-muted-foreground"
			/>
			<Input
				type="search"
				value={input}
				placeholder={text.placeholder}
				aria-label={text.label}
				aria-describedby="filter-help"
				className="pl-9"
				onChange={(event) => setInput(event.target.value)}
				onKeyDown={(event) => {
					if (event.key !== "Escape" || !input) return;
					event.preventDefault();
					event.stopPropagation();
					sent.current = undefined;
					setInput("");
					emit.current(undefined);
				}}
			/>
			<p id="filter-help" className="mt-1 text-sm text-muted-foreground">
				{text.help}
			</p>
		</div>
	);
}

function rowOf({ parent, parent_b64, ...entry }: SearchMatch): EntryRow {
	const dir = pathParts(parent_b64);
	return {
		entry,
		dir,
		pathB64: absolutePath([...dir, decodeB64(entry.name_b64)]),
		display: displayName(`${parent}/${entry.name}`),
		parent: {
			display: displayName(parent || "/"),
			pathB64: dir.length > 0 ? absolutePath(dir) : "",
		},
	};
}

export function SearchResults({
	snapshot,
	path,
	q,
	pathLabel,
	selection,
	onOpen,
	onNoMatch,
}: {
	snapshot: string;
	path: string;
	q: string;
	pathLabel: string;
	selection: Selection;
	onOpen: (pathB64: string) => void;
	onNoMatch: (noMatch: boolean) => void;
}) {
	const [state, setState] = useState(SEARCHING);
	const text = copy.restore.browser.search;

	useEffect(() => {
		const controller = new AbortController();
		const params = new URLSearchParams({ q });
		if (path) params.set("path", path);
		streamNdjson<SearchLine>(
			`/api/v1/snapshots/${encodeURIComponent(snapshot)}/search?${params}`,
			controller.signal,
			(lines) =>
				setState((prev) => {
					let next = prev;
					const rows: EntryRow[] = [];
					for (const line of lines) {
						if ("match" in line) rows.push(rowOf(line.match));
						else if ("done" in line) {
							next = {
								...next,
								status: "done",
								truncated: line.done.truncated,
								count: line.done.count,
							};
						} else next = { ...next, status: "error", error: line.error };
					}
					return rows.length > 0
						? { ...next, rows: [...prev.rows, ...rows] }
						: next;
				}),
		)
			.then(() =>
				setState((prev) =>
					prev.status === "searching" ? { ...prev, status: "error" } : prev,
				),
			)
			.catch((error: Error) => {
				if (controller.signal.aborted) return;
				setState((prev) => ({
					...prev,
					status: "error",
					error: error.message,
				}));
			});
		return () => controller.abort();
	}, [snapshot, path, q]);

	const { rows, status } = state;
	const noMatch = status === "done" && rows.length === 0;
	useEffect(() => onNoMatch(noMatch), [noMatch, onNoMatch]);
	let body: React.ReactNode;
	if (status === "error" && rows.length === 0) {
		body = (
			<Alert variant="destructive" role="alert" className="m-4 w-auto">
				<AlertDescription>{text.failed(state.error)}</AlertDescription>
			</Alert>
		);
	} else if (noMatch) {
		body = (
			<div className="flex flex-col items-start gap-2 px-4 py-8">
				<p className="font-semibold">{text.noMatch(q)}</p>
				<p className="text-sm text-muted-foreground">
					{path ? (
						<TextWithPaths text={text.noMatchBody(pathLabel)} />
					) : (
						text.noMatchRoot
					)}
				</p>
			</div>
		);
	} else {
		body = (
			<>
				{status === "searching" && (
					<p className="flex items-center gap-2 border-b px-4 py-2 text-sm text-muted-foreground">
						<Spinner />
						{text.searching(rows.length)}
					</p>
				)}
				{status === "done" && (
					<p
						aria-hidden
						className="border-b px-4 py-2 text-sm text-muted-foreground"
					>
						{matchCount(state.count)}
					</p>
				)}
				{status === "error" && (
					<Alert variant="destructive" role="alert" className="m-4 w-auto">
						<AlertDescription>{text.failed(state.error)}</AlertDescription>
					</Alert>
				)}
				<EntriesTable rows={rows} selection={selection} onOpen={onOpen} />
				{state.truncated && (
					<p className="border-t px-4 py-2 text-sm text-muted-foreground">
						{text.limit}
					</p>
				)}
			</>
		);
	}
	let announcement = "";
	if (status === "searching") announcement = text.searchingStatus;
	else if (status === "done") {
		announcement =
			rows.length === 0 ? text.noMatch(q) : matchCount(state.count);
	}
	return (
		<>
			<p role="status" className="sr-only">
				{announcement}
			</p>
			{body}
		</>
	);
}
