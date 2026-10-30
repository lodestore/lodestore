import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
	EntryItem,
	EntryPage,
	RestoreHost,
	RestoreSnapshot,
} from "@/api/gen/model";
import { decodeB64, decodeText, encodeB64 } from "@/lib/names";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const HOST = "agent-web";
const SNAP = "a".repeat(64);
const iso = (text: string) => `\u2068${text}\u2069`;
const OTHER = "b".repeat(64);
const enc = (text: string) => encodeB64(new TextEncoder().encode(text));

function entry(
	name: string,
	kind: EntryItem["kind"] = "file",
	over: Partial<EntryItem> = {},
): EntryItem {
	return {
		name,
		name_b64: enc(name),
		kind,
		size: 1536,
		mtime_ms: Date.UTC(2026, 9, 1, 12, 0, 0),
		mode: kind === "dir" ? 0o40755 : 0o100644,
		uid: 1000,
		gid: 1000,
		link_target: null,
		...over,
	};
}

const files = Array.from({ length: 447 }, (_, i) =>
	entry(`file-${String(i + 1).padStart(3, "0")}.log`),
);

const tree: Record<string, EntryItem[]> = {
	"": [
		entry("docs", "dir"),
		entry("empty", "dir", { mtime_ms: 0 }),
		entry("broken", "dir"),
		...files,
	],
	"/docs": [
		entry("deep", "dir"),
		entry("hosts", "symlink", { link_target: "/etc/hosts" }),
		entry("readme.txt"),
		entry("invoice" + String.fromCodePoint(0x202e) + "gpj.exe"),
		entry("two\nlines"),
		entry("備份 😀.txt"),
	],
	"/empty": [],
	"/solo": [entry("only.txt")],
	"/big": Array.from({ length: 1300 }, (_, i) =>
		entry(`big-${String(i + 1).padStart(4, "0")}.log`),
	),
};

const listing = (path: string, offset: number): EntryPage => {
	const all = tree[path] ?? [];
	return {
		path,
		path_b64: path ? enc(path) : "",
		entries: all.slice(offset, offset + 200),
		total: all.length,
		offset,
	};
};

function snapshot(id: string, time: number): RestoreSnapshot {
	return {
		id,
		hostname: "web-1",
		host_key: HOST,
		repository_id: "repo-1",
		time,
		incomplete: false,
		paths: ["/etc"],
		policy_name: "etc-and-home",
		policy_deleted: false,
		received_at: time,
		tags: [],
		warnings_total: 0,
		errors_total: 0,
		stats: {
			bytes_total: 2_097_152,
			bytes_added: 1024,
			bytes_read: 2_097_152,
			dirs: 4,
			files_new: 10,
			files_changed: 5,
			files_unchanged: 1_235,
		},
	};
}

const snapshots = [
	snapshot(SNAP, Date.now()),
	snapshot(OTHER, Date.now() - 1e8),
];
const hosts: RestoreHost[] = [
	{
		key: HOST,
		host_id: HOST,
		hostname: "web-1",
		agent: true,
		status: "online",
		labels: [],
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 2,
		latest_snapshot_ms: snapshots[0].time,
	},
];

function mockBrowser(
	onEntries?: (path: string, offset: number) => Response | undefined,
) {
	server.use(
		http.get("/api/v1/restore/hosts", () => HttpResponse.json(hosts)),
		http.get("/api/v1/restore/snapshots", () =>
			HttpResponse.json({ items: snapshots, total: 2, next: null }),
		),
		http.get("/api/v1/snapshots/:id", ({ params }) =>
			HttpResponse.json(snapshots.find((s) => s.id === params.id)),
		),
		http.get("/api/v1/snapshots/:id/entries", ({ request }) => {
			const url = new URL(request.url);
			const raw = url.searchParams.get("path") ?? "";
			const path = raw ? decodeText(decodeB64(raw)) : "";
			const offset = Number(url.searchParams.get("offset") ?? 0);
			return (
				onEntries?.(path, offset) ?? HttpResponse.json(listing(path, offset))
			);
		}),
	);
}

async function openBrowser(path?: string, select?: string) {
	useSignedIn();
	const search = new URLSearchParams({ host: HOST, snapshot: SNAP });
	if (path) search.set("path", enc(path));
	if (select) search.set("select", enc(select));
	const app = renderApp(`/restore?${search}`);
	await screen.findByRole("heading", { level: 1, name: "Restore" });
	return app;
}

const rows = () => within(screen.getByRole("table")).getAllByRole("row");

beforeEach(() => {
	window.innerWidth = 1440;
});
afterEach(() => {
	window.innerWidth = 1024;
});

const announced = (text: string) =>
	screen.findByText(text, { selector: "[role=status]" });

describe("File browser selection", { timeout: 30_000 }, () => {
	it("counts files and folders together and clears on request", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser();

		expect(
			await screen.findByText(
				"Select files or folders to restore them or download an archive.",
			),
		).toBeVisible();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select docs" }),
		);
		await user.click(
			screen.getByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.click(
			screen.getByRole("checkbox", { name: "Select file-002.log" }),
		);

		await announced("3 items selected");
		expect(
			screen.getByText("Folders include everything inside."),
		).toBeVisible();
		expect(screen.getByRole("checkbox", { name: "Select docs" })).toBeChecked();
		expect(
			screen.getByRole("checkbox", { name: "Select all items in this folder" }),
		).toBePartiallyChecked();

		await user.click(screen.getByRole("button", { name: "Clear selection" }));
		await announced("Selection cleared");
		expect(
			screen.getByRole("checkbox", { name: "Select docs" }),
		).not.toBeChecked();
	});

	it("selects a range with Shift+click and Shift+Space", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser();

		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.keyboard("{Shift>}");
		await user.click(
			screen.getByRole("checkbox", { name: "Select file-004.log" }),
		);
		await user.keyboard("{/Shift}");
		await announced("4 items selected");

		const last = screen.getByRole("checkbox", { name: "Select file-007.log" });
		last.focus();
		await user.keyboard("{Shift>}{ }{/Shift}");
		await announced("7 items selected");
	});

	it("selects the whole folder from the header without listing its pages", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser();

		const all = await screen.findByRole("checkbox", {
			name: "Select all items in this folder",
		});
		await user.click(all);
		await announced("1 item selected");
		expect(all).toBeChecked();
	});

	it("checks the header when every row of a fully loaded folder is selected", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser("/solo", "/solo/only.txt");

		const all = await screen.findByRole("checkbox", {
			name: "Select all items in this folder",
		});
		await waitFor(() => expect(all).toBeChecked());
		await user.click(all);
		await announced("Selection cleared");
		expect(all).not.toBeChecked();
	});

	it("shows the contents of a selected folder as included", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser();

		await user.click(
			await screen.findByRole("checkbox", { name: "Select docs" }),
		);
		await user.click(screen.getByRole("button", { name: "Open folder docs" }));
		await screen.findByRole("heading", { name: "Contents of /docs" });

		const child = await screen.findByRole("checkbox", {
			name: "Select readme.txt",
		});
		expect(child).toBeChecked();
		expect(child).toBeDisabled();
		expect(child).toHaveAttribute("title", "Included with /docs");
		const all = screen.getByRole("checkbox", {
			name: "Select all items in this folder",
		});
		expect(all).toBeChecked();
		expect(all).toBeEnabled();
		await announced("1 item selected");
	});

	it("drops items inside a folder when the folder is selected", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser("/docs");

		await user.click(
			await screen.findByRole("checkbox", { name: "Select readme.txt" }),
		);
		await user.click(screen.getByRole("checkbox", { name: "Select deep" }));
		await announced("2 items selected");
		await user.click(
			screen.getByRole("checkbox", { name: "Select all items in this folder" }),
		);
		await announced("1 item selected");
	});

	it("keeps the selection across folders and drops it when the snapshot changes", async () => {
		mockBrowser();
		const user = userEvent.setup();
		const { router } = await openBrowser();

		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.click(screen.getByRole("button", { name: "Open folder docs" }));
		await screen.findByRole("heading", { name: "Contents of /docs" });
		await announced("1 item selected");

		const region = await announced("1 item selected");
		await router.navigate({
			to: "/restore",
			search: { host: HOST, snapshot: OTHER },
		});
		expect(
			await announced("Selection cleared because the snapshot changed."),
		).toBe(region);
		expect(
			await screen.findByRole("checkbox", { name: "Select docs" }),
		).not.toBeChecked();
	});
});

describe("File browser listing", { timeout: 30_000 }, () => {
	it("shows 200 entries at a time and loads more on request", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser();

		await screen.findByRole("button", { name: "Open folder docs" });
		expect(rows()).toHaveLength(201);
		expect(screen.getByText("Showing 200 of 450 items")).toBeVisible();

		await user.click(screen.getByRole("button", { name: "Show next 200" }));
		await screen.findByText("Showing 400 of 450 items");
		expect(rows()).toHaveLength(401);

		await user.click(screen.getByRole("button", { name: "Show next 200" }));
		await screen.findByText("Showing 450 of 450 items");
		expect(rows()).toHaveLength(451);
		expect(
			screen.queryByRole("button", { name: "Show next 200" }),
		).not.toBeInTheDocument();
		await waitFor(() =>
			expect(screen.getByText("Showing 450 of 450 items")).toHaveFocus(),
		);
	});

	it("opens a folder through the URL and moves focus to the table heading", async () => {
		mockBrowser();
		const user = userEvent.setup();
		const { router } = await openBrowser();

		await user.click(
			await screen.findByRole("button", { name: "Open folder docs" }),
		);

		await waitFor(() =>
			expect(router.state.location.search.path).toBe(enc("/docs")),
		);
		const heading = await screen.findByRole("heading", {
			name: "Contents of /docs",
		});
		await waitFor(() => expect(heading).toHaveFocus());
		expect(screen.getByText("→ /etc/hosts")).toBeVisible();

		const nav = screen.getByRole("navigation", { name: "Path in snapshot" });
		expect(within(nav).getByText("docs")).toHaveAttribute(
			"aria-current",
			"page",
		);

		await user.click(
			within(nav).getByRole("button", { name: "Snapshot root" }),
		);
		await waitFor(() =>
			expect(router.state.location.search.path).toBeUndefined(),
		);
		await waitFor(() =>
			expect(
				screen.getByRole("heading", { name: "Contents of /" }),
			).toHaveFocus(),
		);
	});

	it("collapses a deep path behind a menu", async () => {
		mockBrowser();
		const user = userEvent.setup();
		const { router } = await openBrowser("/a/b/c/d/e/f");

		const nav = await screen.findByRole("navigation", {
			name: "Path in snapshot",
		});
		expect(within(nav).getByText("f")).toHaveAttribute("aria-current", "page");
		expect(within(nav).getByRole("button", { name: "e" })).toBeVisible();
		expect(within(nav).queryByRole("button", { name: "c" })).toBeNull();

		await user.click(
			within(nav).getByRole("button", {
				name: "Show hidden folders in the path",
			}),
		);
		await user.click(await screen.findByRole("menuitem", { name: "c" }));
		await waitFor(() =>
			expect(router.state.location.search.path).toBe(enc("/a/b/c")),
		);
	});

	it("shows unsafe and international names as plain text", async () => {
		mockBrowser();
		await openBrowser("/docs");

		expect(await screen.findByText("invoice[U+202E]gpj.exe")).toBeVisible();
		expect(screen.getByText("two␊lines")).toBeVisible();
		expect(screen.getByText("備份 😀.txt")).toBeVisible();
	});

	it("opens the root for a malformed path in the URL", async () => {
		mockBrowser();
		for (const path of ["a", "abc!", "/"]) {
			useSignedIn();
			renderApp(
				`/restore?host=${HOST}&snapshot=${SNAP}&path=${encodeURIComponent(path)}`,
			);
			expect(
				await screen.findByRole("button", { name: "Open folder docs" }),
			).toBeVisible();
			cleanup();
		}
	});

	it("keeps at most 1,000 rows in the table", async () => {
		mockBrowser();
		const user = userEvent.setup();
		await openBrowser("/big");

		await screen.findByText("big-0001.log");
		for (let page = 2; page <= 6; page += 1) {
			await user.click(screen.getByRole("button", { name: "Show next 200" }));
			await screen.findByText(`big-${String(page * 200).padStart(4, "0")}.log`);
		}
		expect(rows()).toHaveLength(1001);
		expect(screen.getByText("Showing 201–1,200 of 1,300 items")).toBeVisible();
		expect(screen.queryByText("big-0001.log")).toBeNull();

		await user.click(screen.getByRole("button", { name: "Show previous 200" }));
		expect(await screen.findByText("big-0001.log")).toBeVisible();
		expect(rows()).toHaveLength(1001);
		expect(screen.getByText("Showing 1,000 of 1,300 items")).toBeVisible();
		expect(
			screen.queryByRole("button", { name: "Show previous 200" }),
		).toBeNull();
		await waitFor(() =>
			expect(
				screen.getByRole("heading", { name: "Contents of /big" }),
			).toHaveFocus(),
		);
	});

	it("shows a dash for a folder without a modification time", async () => {
		mockBrowser();
		await openBrowser();

		const row = (
			await screen.findByRole("button", { name: "Open folder empty" })
		).closest("tr") as HTMLElement;
		expect(within(row).getAllByText("—")).toHaveLength(2);
		expect(within(row).queryByText(/1970|1969/)).toBeNull();
		const docs = screen
			.getByRole("button", { name: "Open folder docs" })
			.closest("tr") as HTMLElement;
		expect(within(docs).getByText(/2026/)).toBeVisible();
	});

	it("explains an empty folder", async () => {
		mockBrowser();
		await openBrowser("/empty");
		expect(await screen.findByText("This folder is empty.")).toBeVisible();
	});

	it("reports a listing error and retries", async () => {
		let failures = 1;
		mockBrowser((path) => {
			if (path === "" && failures-- > 0) {
				return apiError(502, "storage", "the bucket is unreachable");
			}
		});
		const user = userEvent.setup();
		await openBrowser();

		const alert = await screen.findByRole("alert");
		expect(alert).toHaveTextContent(
			"Couldn't list this folder. The bucket is unreachable. Retry, or open a different folder.",
		);
		await user.click(
			within(alert).getByRole("button", { name: "Retry loading" }),
		);
		expect(
			await screen.findByRole("button", { name: "Open folder docs" }),
		).toBeVisible();
	});
});

function match(name: string, parent = "/docs") {
	return {
		match: {
			...entry(name),
			parent,
			parent_b64: parent ? enc(parent) : "",
		},
	};
}

function ndjson(lines: object[], hold?: Promise<void>) {
	const encoder = new TextEncoder();
	return new HttpResponse(
		new ReadableStream({
			async start(controller) {
				for (const [i, line] of lines.entries()) {
					if (hold && i === 1) await hold;
					controller.enqueue(encoder.encode(`${JSON.stringify(line)}\n`));
				}
				controller.close();
			},
		}),
		{ headers: { "Content-Type": "application/x-ndjson" } },
	);
}

async function openFiltered(q: string) {
	useSignedIn();
	const search = new URLSearchParams({ host: HOST, snapshot: SNAP, q });
	const app = renderApp(`/restore?${search}`);
	await screen.findByRole("heading", { level: 1, name: "Restore" });
	return app;
}

const filterBox = () =>
	screen.getByRole("searchbox", { name: "Filter files by name" });

describe("File browser filter", { timeout: 30_000 }, () => {
	it("shows the first matches before the search is done", async () => {
		mockBrowser();
		let release: () => void = () => {};
		const hold = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson(
					[
						match("one.log"),
						match("two.log"),
						{ done: { count: 2, truncated: false } },
					],
					hold,
				),
			),
		);
		await openFiltered("log");

		expect(await screen.findByText("one.log")).toBeVisible();
		expect(screen.getByText("Searching… 1 found")).toBeVisible();
		await announced("Searching…");
		expect(screen.queryByText("two.log")).toBeNull();

		release();
		expect(await screen.findByText("two.log")).toBeVisible();
		await screen.findByText("2 matches", { selector: "[role=status]" });
		expect(screen.queryByText(/^Searching/)).toBeNull();
		expect(
			screen.getByText(`Results for "${iso("log")}" in ${iso("/")}`),
		).toBeVisible();
	});

	it("explains the 500 match limit", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([match("one.log"), { done: { count: 500, truncated: true } }]),
			),
		);
		await openFiltered("log");

		expect(
			await screen.findByText(
				"Showing the first 500 matches. Narrow the filter or open a deeper folder.",
			),
		).toBeVisible();
	});

	it("cancels the previous search when the filter changes", async () => {
		mockBrowser();
		const signals: AbortSignal[] = [];
		const realFetch = globalThis.fetch;
		vi.spyOn(globalThis, "fetch").mockImplementation((input, init) => {
			if (String(input).includes("/search")) {
				signals.push(init?.signal as AbortSignal);
			}
			return realFetch(input, init);
		});
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson(
					[match("one.log"), { done: { count: 1, truncated: false } }],
					new Promise(() => {}),
				),
			),
		);
		const user = userEvent.setup();
		await openFiltered("lo");
		await screen.findByText("one.log");
		expect(signals).toHaveLength(1);
		expect(signals[0].aborted).toBe(false);

		await user.type(filterBox(), "g");
		await waitFor(() => expect(signals).toHaveLength(2), { timeout: 5000 });
		expect(signals[0].aborted).toBe(true);
		expect(signals[1].aborted).toBe(false);
		expect(filterBox()).toHaveValue("log");
		vi.restoreAllMocks();
	});

	it("opens the parent folder and clears the filter", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([match("one.log"), { done: { count: 1, truncated: false } }]),
			),
		);
		const user = userEvent.setup();
		await openFiltered("log");

		await user.click(
			await screen.findByRole("button", { name: "Open folder /docs" }),
		);

		await screen.findByRole("heading", { name: "Contents of /docs" });
		expect(filterBox()).toHaveValue("");
		expect(screen.queryByText(/^Results for/)).toBeNull();
		expect(
			await screen.findByRole("button", { name: "Open folder deep" }),
		).toBeVisible();
	});

	it("escapes the folder path in the results header", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([{ done: { count: 0, truncated: false } }]),
			),
		);
		useSignedIn();
		const search = new URLSearchParams({
			host: HOST,
			snapshot: SNAP,
			path: enc(`/a${String.fromCodePoint(0x202e)}b`),
			q: "x\ny",
		});
		renderApp(`/restore?${search}`);

		expect(
			await screen.findByText(
				`Results for "${iso("x␊y")}" in ${iso("/a[U+202E]b")}`,
			),
		).toBeVisible();
		expect(await screen.findByText(/or below has that name/)).toHaveTextContent(
			`Nothing in ${iso("/a[U+202E]b")} or below has that name. Check the spelling, or filter from a higher folder.`,
		);
	});

	it("moves focus to the table heading when the match is in the current folder", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([
					match("one.log", ""),
					{ done: { count: 1, truncated: false } },
				]),
			),
		);
		const user = userEvent.setup();
		await openFiltered("log");

		await user.click(
			await screen.findByRole("button", { name: "Open folder /" }),
		);

		await waitFor(() =>
			expect(
				screen.getByRole("heading", { name: "Contents of /" }),
			).toHaveFocus(),
		);
	});

	it("says when nothing matches and clears with Escape", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([{ done: { count: 0, truncated: false } }]),
			),
		);
		const user = userEvent.setup();
		await openFiltered("zzz");

		expect(
			await screen.findByText(`No match for "${iso("zzz")}"`, {
				selector: ":not([role=status])",
			}),
		).toBeVisible();
		await announced(`No match for "${iso("zzz")}"`);
		expect(
			screen.getByText(
				"Nothing in this snapshot has that name. Check the spelling.",
			),
		).toBeVisible();
		expect(
			screen.getAllByRole("button", { name: "Clear filter" }),
		).toHaveLength(1);
		expect(
			screen.queryByText(
				"Select files or folders to restore them or download an archive.",
			),
		).toBeNull();

		filterBox().focus();
		await user.keyboard("{Escape}");
		await waitFor(() => expect(filterBox()).toHaveValue(""));
		expect(
			await screen.findByRole("button", { name: "Open folder docs" }),
		).toBeVisible();
	});

	it("treats a stream that ends without a summary as failed", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([match("one.log")]),
			),
		);
		await openFiltered("log");

		expect(await screen.findByText("one.log")).toBeVisible();
		expect(
			await screen.findByText(
				"Couldn't search this snapshot. Try again, or filter from another folder.",
			),
		).toBeVisible();
		expect(screen.queryByText("1 match")).toBeNull();
	});

	it("sends an expired session to login", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				apiError(401, "unauthorized", "authentication required"),
			),
		);
		const { router } = await openFiltered("log");

		await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
	});

	it("shows a search failure", async () => {
		mockBrowser();
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				apiError(502, "storage", "the bucket is unreachable"),
			),
		);
		await openFiltered("log");

		expect(
			await screen.findByText(
				"Couldn't search this snapshot. The bucket is unreachable. Try again, or filter from another folder.",
			),
		).toBeVisible();
	});

	it("shows unicode and control characters safely and selects a match", async () => {
		mockBrowser();
		const tricky = `invoice${String.fromCodePoint(0x202e)}gpj.exe`;
		server.use(
			http.get("/api/v1/snapshots/:id/search", () =>
				ndjson([
					match("備份 😀.txt"),
					match(tricky),
					{ done: { count: 2, truncated: false } },
				]),
			),
		);
		const user = userEvent.setup();
		await openFiltered("備");

		expect(await screen.findByText("備份 😀.txt")).toBeVisible();
		expect(screen.getByText("invoice[U+202E]gpj.exe")).toBeVisible();
		await user.click(
			screen.getByRole("checkbox", { name: "Select 備份 😀.txt" }),
		);
		await announced("1 item selected");
		expect(screen.getByRole("button", { name: "Restore…" })).toBeVisible();
	});
});
