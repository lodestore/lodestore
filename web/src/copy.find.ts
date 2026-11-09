import { isolate } from "@/lib/names";

const noun = (n: number, one: string, many: string) =>
	`${n} ${n === 1 ? one : many}`;

const span = (first: string, last: string) =>
	first === last ? first : `${first} – ${last}`;

export const find = {
	input: {
		label: "File name or pattern",
		placeholder: "Name or glob, e.g. *.conf or /etc/nginx/**",
	},
	submit: "Search",
	filters: {
		allHosts: "All hosts",
		anyTime: "Any time",
		periods: {
			week: "Last 7 days",
			month: "Last 30 days",
			custom: "Custom range",
		},
		from: "From",
		to: "To",
	},
	scope:
		"Searches every snapshot that is still kept, including hosts without an agent.",
	status: {
		searching: "Searching",
		running: (counts: {
			scanned: number;
			total: number;
			searched_hosts: number;
			files: number;
			versions: number;
		}) =>
			`Searching · ${counts.scanned} of ${noun(counts.total, "snapshot", "snapshots")} on ${noun(counts.searched_hosts, "host", "hosts")} · ${noun(counts.files, "file", "files")}, ${noun(counts.versions, "version", "versions")} so far`,
		done: (counts: {
			total: number;
			searched_hosts: number;
			duration: string;
			files: number;
			versions: number;
		}) =>
			`Done · searched ${noun(counts.total, "snapshot", "snapshots")} on ${noun(counts.searched_hosts, "host", "hosts")} in ${counts.duration} · ${noun(counts.files, "file", "files")}, ${noun(counts.versions, "version", "versions")}`,
		limit: (limit: number) =>
			`Stopped at ${limit} matches. Showing what was found. Narrow the pattern or add filters.`,
		timeout: (duration: string) =>
			`Stopped after ${duration}. Showing what was found. Narrow the pattern or add filters.`,
		walkLimit:
			"Stopped a snapshot whose directory tree is larger than one search may walk. Showing what was found in the other snapshots.",
		cancelled: "Search stopped. Showing what was found.",
		failed: (reason: string) => `Couldn't finish the search: ${reason}`,
		progress: (scanned: number, total: number) =>
			`${scanned} of ${noun(total, "snapshot", "snapshots")}`,
	},
	skipped: (n: number) =>
		`${noun(n, "snapshot", "snapshots")} couldn't be read`,
	stop: "Stop",
	list: { label: "Matching files" },
	versions: (n: number) => noun(n, "version", "versions"),
	lastSeen: (date: string) => `Last seen ${date}`,
	gone: {
		heading: (date: string) => `Not in snapshots after ${date}.`,
		body: (next: string) =>
			`The file was deleted or moved before the ${next} backup. Restore the last version to bring it back.`,
	},
	range: (first: string, last: string) => span(first, last),
	version: {
		separator: " · ",
		modified: (time: string) => `modified ${time}`,
		present: (n: number) => `Present in ${noun(n, "snapshot", "snapshots")}`,
		all: (n: number) => `All ${n} snapshots`,
		restore: "Restore…",
		restoreLabel: (path: string, host: string, first: string, last: string) =>
			`Restore ${path} from ${isolate(host)}, version ${span(first, last)}`,
		show: "Show in snapshot",
		showLabel: (path: string, time: string, host: string) =>
			`Show ${path} in the ${time} snapshot of ${isolate(host)}`,
		download: "Download…",
		downloadLabel: (path: string, host: string, first: string, last: string) =>
			`Download ${path} from ${isolate(host)}, version ${span(first, last)}`,
	},
	back: "Back to results",
	empty: {
		heading: "Find a file on any host",
		body: "Search by name or glob across every snapshot that is still kept.",
		examples: ["nginx.conf", "*.sql.gz", "/etc/nginx/**"],
	},
	noSnapshots: {
		heading: "No snapshots match these filters.",
		body: "Choose other hosts or labels, or widen the period.",
	},
	none: {
		heading: (pattern: string) => `No files match “${pattern}”.`,
		body: "Check the spelling, use * for any part of a name, or widen the period.",
	},
} as const;
