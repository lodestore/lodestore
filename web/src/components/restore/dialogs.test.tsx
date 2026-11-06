import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { EntryItem, RestoreHost, RestoreSnapshot } from "@/api/gen/model";
import { encodeB64 } from "@/lib/names";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

Element.prototype.hasPointerCapture ??= () => false;
Element.prototype.setPointerCapture ??= () => {};
Element.prototype.releasePointerCapture ??= () => {};
Element.prototype.scrollIntoView ??= () => {};

const HOST = "agent-web";
const SNAP = "a".repeat(64);
const iso = (text: string) => `\u2068${text}\u2069`;
const STARTED = "Download started. The archive streams to your browser.";
const enc = (text: string) => encodeB64(new TextEncoder().encode(text));

const entry = (name: string): EntryItem => ({
	name,
	name_b64: enc(name),
	kind: "file",
	size: 1536,
	mtime_ms: Date.UTC(2026, 9, 1, 12, 0, 0),
	mode: 0o100644,
	uid: 1000,
	gid: 1000,
	link_target: null,
});

const snapshot: RestoreSnapshot = {
	id: SNAP,
	hostname: "web-1.local",
	host_key: HOST,
	repository_id: "repo-1",
	time: Date.UTC(2026, 9, 14, 23, 30),
	incomplete: false,
	paths: ["/etc"],
	policy_name: "etc",
	policy_deleted: false,
	received_at: Date.UTC(2026, 9, 14, 23, 31),
	verified_by: "server",
	tags: [],
	warnings_total: 0,
	errors_total: 0,
	stats: {
		bytes_total: 2_097_152,
		bytes_added: 1024,
		bytes_read: 2_097_152,
		dirs: 1,
		files_new: 2,
		files_changed: 0,
		files_unchanged: 0,
	},
};

const host = (over: Partial<RestoreHost>): RestoreHost => ({
	key: HOST,
	host_id: HOST,
	hostname: "web-1",
	agent: true,
	status: "online",
	labels: [],
	repository_id: "repo-1",
	repository_name: "main",
	snapshot_count: 1,
	latest_snapshot_ms: snapshot.time,
	unverified_count: 0,
	restore_roots: [],
	...over,
});

const fleet: RestoreHost[] = [
	host({}),
	host({
		key: "agent-db",
		host_id: "agent-db",
		hostname: "db-1",
		repository_id: "repo-2",
	}),
	host({
		key: "agent-off",
		host_id: "agent-off",
		hostname: "cold-1",
		status: "offline",
	}),
	host({
		key: "agent-gone",
		host_id: "agent-gone",
		hostname: "gone-1",
		status: "revoked",
	}),
];

function mockServer(hosts = fleet, shown = snapshot) {
	server.use(
		http.get("/api/v1/restore/hosts", () => HttpResponse.json(hosts)),
		http.get("/api/v1/restore/snapshots", () =>
			HttpResponse.json({
				items: [shown],
				total: 1,
				next: null,
				latest_verified_id: shown.verified_by == null ? null : shown.id,
			}),
		),
		http.get("/api/v1/snapshots/:id", () => HttpResponse.json(shown)),
		http.get("/api/v1/snapshots/:id/entries", () =>
			HttpResponse.json({
				path: "",
				path_b64: "",
				entries: [entry("file-001.log"), entry("file-002.log")],
				total: 2,
				offset: 0,
			}),
		),
		http.post("/api/v1/snapshots/:id/summary", () =>
			HttpResponse.json({ files: 12, dirs: 0, bytes: 1_048_576 }),
		),
	);
}

const unsigned: RestoreSnapshot = { ...snapshot, verified_by: null };

async function openRestore() {
	useSignedIn();
	const app = renderApp(`/restore?host=${HOST}&snapshot=${SNAP}`);
	const user = userEvent.setup();
	await user.click(
		await screen.findByRole("checkbox", { name: "Select file-001.log" }),
	);
	await user.click(screen.getByRole("button", { name: "Restore…" }));
	const dialog = await screen.findByRole("dialog");
	return { app, user, dialog };
}

beforeEach(() => {
	window.innerWidth = 1440;
});
afterEach(() => {
	window.innerWidth = 1024;
	toast.dismiss();
});

describe("Restore dialog", { timeout: 30_000 }, () => {
	it("starts with safe defaults and the snapshot date in the folder", async () => {
		mockServer();
		const { dialog, user } = await openRestore();
		const view = within(dialog);

		expect(view.getByRole("heading", { name: "Restore 1 item" })).toBeVisible();
		const day = new Intl.DateTimeFormat("en-CA").format(snapshot.time);
		expect(view.getByRole("textbox", { name: "Folder on web-1" })).toHaveValue(
			`/var/lodestore/restore/${day}-aaaaaaaa`,
		);
		expect(view.getByText("Source host")).toBeVisible();
		expect(view.getByText("Destination")).toBeVisible();
		expect(view.getByRole("radiogroup", { name: "Destination" })).toBeVisible();
		expect(view.getByRole("combobox", { name: "Host: web-1" })).toBeVisible();
		expect(view.queryByRole("region", { name: "Summary" })).toBeNull();
		expect(dialog).not.toHaveAttribute("aria-describedby");
		expect(
			view.getByRole("radio", { name: /New folder/ }),
		).toHaveAccessibleDescription("Recommended. Nothing existing is touched.");
		expect(
			view.getByRole("radio", { name: "Original location" }),
		).not.toHaveAccessibleDescription();
		const options = view.getByRole("button", { name: /^Options/ });
		expect(options).toHaveAttribute("aria-expanded", "false");
		expect(options).toHaveTextContent(
			"Overwrite changed files · Original owners",
		);
		expect(
			view.queryByRole("combobox", { name: "If a file already exists" }),
		).toBeNull();
		await user.click(options);
		const conflict = view.getByRole("combobox", {
			name: "If a file already exists",
		});
		expect(conflict).toHaveTextContent("Overwrite changed files");
		expect(conflict).not.toHaveAccessibleDescription();
		const owners = view.getByRole("switch", {
			name: "Restore original owners",
		});
		expect(owners).toBeChecked();
		await user.click(owners);
		expect(options).toHaveTextContent(
			"Overwrite changed files · Agent user (root)",
		);
		expect(view.getByRole("button", { name: "Start restore" })).toBeEnabled();
		expect(await view.findByText("12 files · 1 MiB")).toBeVisible();
	});

	it("warns about another host and another repository", async () => {
		mockServer();
		const { dialog, user } = await openRestore();

		await user.click(
			within(dialog).getByRole("combobox", { name: "Host: web-1" }),
		);
		expect(screen.queryByRole("option", { name: /gone-1/ })).toBeNull();
		await user.click(await screen.findByRole("option", { name: /db-1/ }));

		const note = await within(dialog).findByText(
			`${iso("db-1")} will receive files from ${iso("web-1")}.`,
			{ exact: false },
		);
		expect(note).toHaveTextContent("It uses another repository.");
	});

	it("explains that an offline target starts later", async () => {
		mockServer();
		const { dialog, user } = await openRestore();

		await user.click(
			within(dialog).getByRole("combobox", { name: "Host: web-1" }),
		);
		await user.click(await screen.findByRole("option", { name: /cold-1/ }));

		expect(
			await within(dialog).findByText(
				`${iso("cold-1")} is offline. The restore starts when it connects, within 24 hours.`,
			),
		).toBeVisible();
	});

	it("needs a confirmation for the original location and sends in_place", async () => {
		mockServer();
		let body: unknown;
		server.use(
			http.post("/api/v1/restores", async ({ request }) => {
				body = await request.json();
				return HttpResponse.json({ job_id: "job-1" }, { status: 201 });
			}),
		);
		const { dialog, user } = await openRestore();
		const view = within(dialog);

		await user.click(view.getByRole("radio", { name: /Original location/ }));
		const start = view.getByRole("button", { name: "Restore in place" });
		expect(start).toBeDisabled();
		await user.click(
			view.getByRole("checkbox", {
				name: `I understand this writes into the original paths on ${iso("web-1")}.`,
			}),
		);
		expect(start).toBeEnabled();
		await user.click(start);

		await waitFor(() =>
			expect(body).toEqual({
				snapshot_id: SNAP,
				items: [enc("/file-001.log")],
				target_host_id: HOST,
				destination: "/",
				in_place: true,
				conflict: "overwrite_changed",
				restore_owners: true,
				accept_unverified: false,
			}),
		);
	});

	it("unverified restore needs confirmation and sends accept_unverified", async () => {
		mockServer(fleet, unsigned);
		let body: { accept_unverified?: boolean; in_place?: boolean } = {};
		server.use(
			http.post("/api/v1/restores", async ({ request }) => {
				body = (await request.json()) as typeof body;
				return HttpResponse.json({ job_id: "job-1" }, { status: 201 });
			}),
		);
		const { dialog, user } = await openRestore();
		const view = within(dialog);

		const start = view.getByRole("button", { name: "Start restore" });
		expect(start).toBeDisabled();
		await user.click(
			view.getByRole("checkbox", {
				name: "Restore this unsigned snapshot into another folder",
			}),
		);
		expect(start).toBeEnabled();
		await user.click(start);

		await waitFor(() =>
			expect(body).toMatchObject({ in_place: false, accept_unverified: true }),
		);
	});

	it("in place is not offered for an unverified snapshot", async () => {
		mockServer(fleet, unsigned);
		const { dialog } = await openRestore();
		const view = within(dialog);

		expect(
			view.getByRole("radio", { name: /Original location/ }),
		).toBeDisabled();
		expect(
			view.getByText(
				"An unsigned snapshot cannot be restored over the original files.",
			),
		).toBeVisible();
	});

	it("offers no stop-if-not-empty policy for the original location", async () => {
		mockServer();
		const { dialog, user } = await openRestore();
		const view = within(dialog);
		await user.click(view.getByRole("button", { name: /^Options/ }));
		const conflict = view.getByRole("combobox", {
			name: "If a file already exists",
		});

		await user.click(conflict);
		await user.click(
			await screen.findByRole("option", {
				name: "Stop if the folder is not empty",
			}),
		);
		expect(conflict).toHaveTextContent("Stop if the folder is not empty");

		await user.click(view.getByRole("radio", { name: /Original location/ }));
		expect(conflict).toHaveTextContent("Overwrite changed files");
		await user.click(conflict);
		expect(
			screen.queryByRole("option", { name: "Stop if the folder is not empty" }),
		).toBeNull();
	});

	it("shows an error under the folder field for a relative path", async () => {
		mockServer();
		const { dialog, user } = await openRestore();
		const view = within(dialog);
		const field = view.getByRole("textbox", { name: "Folder on web-1" });

		await user.clear(field);
		await user.type(field, "tmp/restore");

		expect(
			view.getByText('Enter an absolute path that starts with "/".'),
		).toBeVisible();
		expect(field).toHaveAttribute("aria-invalid", "true");
		expect(view.getByRole("button", { name: "Start restore" })).toBeDisabled();
	});

	it("rejects a new folder that resolves to the root", async () => {
		mockServer();
		const { dialog, user } = await openRestore();
		const view = within(dialog);
		const field = view.getByRole("textbox", { name: "Folder on web-1" });

		for (const root of ["/", "//", "/.", "/./"]) {
			await user.clear(field);
			await user.type(field, root);
			expect(
				view.getByText(
					'Choose a folder other than "/". To write to the original paths, use Original location.',
				),
			).toBeVisible();
			expect(field).toHaveAttribute("aria-invalid", "true");
			expect(
				view.getByRole("button", { name: "Start restore" }),
			).toBeDisabled();
		}
		await user.clear(field);
		await user.type(field, "/srv/restore");
		expect(view.getByRole("button", { name: "Start restore" })).toBeEnabled();
	});

	it("rejects .. in the folder and sends it without . parts", async () => {
		mockServer();
		let body: { destination?: string } = {};
		server.use(
			http.post("/api/v1/restores", async ({ request }) => {
				body = (await request.json()) as typeof body;
				return HttpResponse.json({ job_id: "job-1" }, { status: 201 });
			}),
		);
		const { dialog, user } = await openRestore();
		const view = within(dialog);
		const field = view.getByRole("textbox", { name: "Folder on web-1" });

		for (const dotdot of ["/tmp/../etc", "/tmp/..", "/srv/a/../"]) {
			await user.clear(field);
			await user.type(field, dotdot);
			expect(
				view.getByText(
					'Enter the path without "..". Name each folder on the way.',
				),
			).toBeVisible();
			expect(field).toHaveAttribute("aria-invalid", "true");
			expect(
				view.getByRole("button", { name: "Start restore" }),
			).toBeDisabled();
		}

		await user.clear(field);
		await user.type(field, "/srv/./restore//");
		await user.click(view.getByRole("button", { name: "Start restore" }));
		await waitFor(() => expect(body.destination).toBe("/srv/restore"));
	});

	it("asks for the in-place confirmation again after the host or mode changes", async () => {
		mockServer();
		const { dialog, user } = await openRestore();
		const view = within(dialog);

		await user.click(view.getByRole("radio", { name: /Original location/ }));
		await user.click(
			view.getByRole("checkbox", {
				name: `I understand this writes into the original paths on ${iso("web-1")}.`,
			}),
		);
		expect(
			view.getByRole("button", { name: "Restore in place" }),
		).toBeEnabled();

		await user.click(view.getByRole("combobox", { name: "Host: web-1" }));
		await user.click(await screen.findByRole("option", { name: /db-1/ }));
		expect(
			await view.findByRole("checkbox", {
				name: `I understand this writes into the original paths on ${iso("db-1")}.`,
			}),
		).not.toBeChecked();
		expect(
			view.getByRole("button", { name: "Restore in place" }),
		).toBeDisabled();

		await user.click(
			view.getByRole("checkbox", {
				name: `I understand this writes into the original paths on ${iso("db-1")}.`,
			}),
		);
		await user.click(view.getByRole("radio", { name: /New folder/ }));
		await user.click(view.getByRole("radio", { name: /Original location/ }));
		expect(
			view.getByRole("checkbox", {
				name: `I understand this writes into the original paths on ${iso("db-1")}.`,
			}),
		).not.toBeChecked();
	});

	it("still lets the restore start when counting fails", async () => {
		mockServer();
		server.use(
			http.post("/api/v1/snapshots/:id/summary", () =>
				apiError(500, "internal", "boom"),
			),
		);
		const { dialog } = await openRestore();

		expect(
			await within(dialog).findByText(
				"Couldn't count the files. You can still start the restore.",
			),
		).toBeVisible();
		expect(
			within(dialog).getByRole("button", { name: "Start restore" }),
		).toBeEnabled();
	});

	it("queues the restore, closes the dialog and keeps the selection", async () => {
		mockServer();
		server.use(
			http.post("/api/v1/restores", () =>
				HttpResponse.json({ job_id: "job-7" }, { status: 201 }),
			),
		);
		const { dialog, user } = await openRestore();

		await user.click(
			within(dialog).getByRole("button", { name: "Start restore" }),
		);

		expect(
			(await screen.findAllByText(`Restore queued for ${iso("web-1")}.`))[0],
		).toBeVisible();
		expect(screen.getByRole("button", { name: "View job" })).toBeVisible();
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(
			screen.getByRole("checkbox", { name: "Select file-001.log" }),
		).toBeChecked();
		await waitFor(() =>
			expect(screen.getByRole("button", { name: "Restore…" })).toHaveFocus(),
		);
	});

	it("stays open while the restore request is pending", async () => {
		mockServer();
		let release = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.post("/api/v1/restores", async () => {
				await gate;
				return HttpResponse.json({ job_id: "job-8" }, { status: 201 });
			}),
		);
		const { dialog, user } = await openRestore();

		await user.click(
			within(dialog).getByRole("button", { name: "Start restore" }),
		);
		await user.keyboard("{Escape}");
		expect(screen.getByRole("dialog")).toBeVisible();

		release();
		expect(
			(await screen.findAllByText(`Restore queued for ${iso("web-1")}.`))[0],
		).toBeVisible();
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
	});

	it("keeps the dialog open and shows the server error", async () => {
		mockServer();
		server.use(
			http.post("/api/v1/restores", () =>
				apiError(500, "internal", "queue unavailable"),
			),
		);
		const { dialog, user } = await openRestore();

		await user.click(
			within(dialog).getByRole("button", { name: "Start restore" }),
		);

		expect(
			await within(dialog).findByText(
				/Couldn't start the restore\. Queue unavailable\. Try again\./,
			),
		).toBeVisible();
		expect(screen.getByRole("dialog")).toBeVisible();
	});

	it("waits for the snapshot details before the dialogs can open", async () => {
		mockServer();
		let release = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.get("/api/v1/snapshots/:id", async () => {
				await gate;
				return HttpResponse.json(snapshot);
			}),
		);
		useSignedIn();
		renderApp(`/restore?host=${HOST}&snapshot=${SNAP}`);
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);

		expect(screen.getByRole("button", { name: "Restore…" })).toBeDisabled();
		expect(screen.getByRole("button", { name: "Download…" })).toBeDisabled();
		release();
		await waitFor(() =>
			expect(screen.getByRole("button", { name: "Restore…" })).toBeEnabled(),
		);
		expect(screen.getByRole("button", { name: "Download…" })).toBeEnabled();
	});

	it("disables Restore… when no host has an agent", async () => {
		mockServer([host({ agent: false, host_id: null, status: null })]);
		useSignedIn();
		renderApp(`/restore?host=${HOST}&snapshot=${SNAP}`);
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);

		expect(screen.getByRole("button", { name: "Restore…" })).toBeDisabled();
		await user.hover(
			screen.getByRole("button", { name: "Restore…" })
				.parentElement as HTMLElement,
		);
		expect(
			(await screen.findAllByText("Add a host to restore to."))[0],
		).toBeVisible();
	});
});

describe("Download dialog", { timeout: 30_000 }, () => {
	async function openDownload(shown = snapshot) {
		mockServer(fleet, shown);
		useSignedIn();
		renderApp(`/restore?host=${HOST}&snapshot=${SNAP}`);
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.click(screen.getByRole("button", { name: "Download…" }));
		return { user, dialog: await screen.findByRole("dialog") };
	}

	const frames = () =>
		Array.from(document.querySelectorAll<HTMLIFrameElement>("iframe[hidden]"));

	afterEach(() => {
		for (const frame of frames()) frame.remove();
	});

	it("download of an unverified snapshot needs confirmation", async () => {
		const { dialog, user } = await openDownload(unsigned);
		const view = within(dialog);

		const start = view.getByRole("button", { name: "Download" });
		expect(start).toBeDisabled();
		await user.click(
			view.getByRole("checkbox", { name: "Download this unsigned snapshot" }),
		);
		expect(start).toBeEnabled();
	});

	it("defaults to tar.gz and changes the file name with the format", async () => {
		const { dialog, user } = await openDownload();
		const view = within(dialog);

		expect(
			view.getByRole("heading", { name: "Download 1 item" }),
		).toBeVisible();
		expect(view.getByRole("radio", { name: "tar.gz" })).toBeChecked();
		expect(view.getByText("web-1-aaaaaaaa.tar.gz")).toBeVisible();
		expect(
			view.getByText(
				"There is no size limit and no resume. If the download stops, start it again.",
			),
		).toBeVisible();

		await user.click(view.getByRole("radio", { name: "zip" }));
		expect(view.getByText("web-1-aaaaaaaa.zip")).toBeVisible();
	});

	it("creates a ticket and hands the archive URL to the browser", async () => {
		let body: unknown;
		const { dialog, user } = await openDownload();
		server.use(
			http.post("/api/v1/downloads", async ({ request }) => {
				body = await request.json();
				return HttpResponse.json(
					{
						id: "dl-1",
						url: "/api/v1/downloads/dl-1/archive",
						file_name: "web-1-12345678.tar.zst",
					},
					{ status: 201 },
				);
			}),
		);

		await user.click(within(dialog).getByRole("radio", { name: "tar.zst" }));
		await user.click(within(dialog).getByRole("button", { name: "Download" }));

		await waitFor(() =>
			expect(frames().map((frame) => frame.getAttribute("src"))).toEqual([
				"/api/v1/downloads/dl-1/archive",
			]),
		);
		expect(body).toEqual({
			snapshot_id: SNAP,
			items: [enc("/file-001.log")],
			format: "tar_zst",
		});
		expect(
			(
				await screen.findAllByText(
					"Download started. The archive streams to your browser.",
				)
			)[0],
		).toBeVisible();
		expect(screen.getByRole("button", { name: "View job" })).toBeVisible();
		expect(
			document
				.querySelector<HTMLElement>("ol[data-sonner-toaster]")
				?.style.getPropertyValue("--offset-bottom"),
		).toBe("88px");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		await waitFor(() =>
			expect(screen.getByRole("button", { name: "Download…" })).toHaveFocus(),
		);
	});

	it("shows a destructive toast when the download cannot start", async () => {
		const { dialog, user } = await openDownload();
		server.use(
			http.post("/api/v1/downloads", () =>
				apiError(422, "invalid", "The selection is empty"),
			),
		);

		await user.click(within(dialog).getByRole("button", { name: "Download" }));

		expect(
			(
				await screen.findAllByText(
					"Couldn't start the download. The selection is empty. Try again.",
				)
			)[0],
		).toBeVisible();
		expect(frames()).toEqual([]);
		expect(screen.getByRole("dialog")).toBeVisible();
	});

	it("keeps the app and shows the error when the archive request fails", async () => {
		const { dialog, user } = await openDownload();
		server.use(
			http.post("/api/v1/downloads", () =>
				HttpResponse.json(
					{ id: "dl-2", url: "/api/v1/downloads/dl-2/archive", file_name: "x" },
					{ status: 201 },
				),
			),
		);

		await user.click(within(dialog).getByRole("button", { name: "Download" }));
		await waitFor(() => expect(frames()).toHaveLength(1));
		expect((await screen.findAllByText(STARTED))[0]).toBeVisible();
		const [frame] = frames();
		const doc = frame.contentDocument as Document;
		doc.open();
		doc.write(
			`<pre>${JSON.stringify({
				code: "not_found",
				message: "This download link was already used or expired.",
			})}</pre>`,
		);
		doc.close();
		frame.dispatchEvent(new Event("load"));

		expect(
			(
				await screen.findAllByText(
					"Couldn't start the download. This download link was already used or expired. Try again.",
				)
			)[0],
		).toBeVisible();
		await waitFor(() => expect(screen.queryAllByText(STARTED)).toEqual([]));
		expect(frames()).toEqual([]);
		expect(
			screen.getByRole("heading", { level: 1, name: "Restore" }),
		).toBeVisible();
	});

	it("shows the error when the browser refuses or cannot load the archive", async () => {
		const { dialog, user } = await openDownload();
		server.use(
			http.post("/api/v1/downloads", () =>
				HttpResponse.json(
					{ id: "dl-3", url: "/api/v1/downloads/dl-3/archive", file_name: "x" },
					{ status: 201 },
				),
			),
		);

		await user.click(within(dialog).getByRole("button", { name: "Download" }));
		await waitFor(() => expect(frames()).toHaveLength(1));
		expect((await screen.findAllByText(STARTED))[0]).toBeVisible();
		const [frame] = frames();
		Object.defineProperty(frame, "contentDocument", { value: null });
		frame.dispatchEvent(new Event("load"));

		expect(
			(
				await screen.findAllByText("Couldn't start the download. Try again.")
			)[0],
		).toBeVisible();
		await waitFor(() => expect(screen.queryAllByText(STARTED)).toEqual([]));
		expect(frames()).toEqual([]);
	});
});
