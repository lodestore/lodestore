import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type {
	HostDetail,
	RestoreHost,
	RestoreSnapshot,
	SnapshotItem,
	SuspicionDto,
} from "@/api/gen/model";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;
const ID = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
const SHORT = "a1b2c3d4";

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};
Element.prototype.hasPointerCapture ??= () => false;
Element.prototype.setPointerCapture ??= () => {};
Element.prototype.releasePointerCapture ??= () => {};
Element.prototype.scrollIntoView ??= () => {};

const detail: HostDetail = {
	id: "web10000-0000-4000-8000-000000000000",
	hostname: "web-1",
	os: "linux",
	arch: "arm64",
	distro: null,
	version: "0.1.0",
	status: "online",
	labels: [],
	repository_id: "repo-1",
	enrolled_at: Date.now() - 30 * DAY,
	last_seen_at: Date.now() - 20_000,
	cert_not_after: Date.now() + 60 * DAY,
	cert_issued_at: Date.now() - 30 * DAY,
	next_renewal_at: Date.now() + 30 * DAY,
	repository_name: "main",
	kernel: "6.8.0-45-generic",
	uptime_s: 3_600,
	mounts: [],
};

function makeSnapshot(over: Partial<SnapshotItem> = {}): SnapshotItem {
	return {
		id: ID,
		hostname: "web-1",
		time: Date.now() - 7_200_000,
		paths: ["/etc"],
		tags: [],
		policy_name: "etc",
		policy_deleted: false,
		received_at: Date.now() - 7_100_000,
		verified_by: "server",
		incomplete: false,
		errors_total: 0,
		warnings_total: 0,
		stats: {
			bytes_added: 100,
			bytes_read: 2048,
			bytes_total: 2048,
			dirs: 1,
			files_new: 2,
			files_changed: 0,
			files_unchanged: 1,
		},
		...over,
	};
}

const automatic: SuspicionDto = {
	automatic: true,
	marked_by: "anomaly",
	marked_at: Date.now(),
	note: null,
	reasons: [
		{ kind: "files_drop", pct: 62, from: 12_480, to: 4_742, factor: 2.6 },
		{ kind: "added_spike", pct: 0, from: 0, to: 5_368_709_120, factor: 12.4 },
	],
};

const manual: SuspicionDto = {
	automatic: false,
	marked_by: "admin",
	marked_at: Date.now(),
	note: "Ransomware note found",
	reasons: [],
};

function mockHost(items: SnapshotItem[]) {
	server.use(
		http.get(`/api/v1/hosts/${detail.id}`, () => HttpResponse.json(detail)),
		http.get(`/api/v1/hosts/${detail.id}/policies`, () =>
			HttpResponse.json([]),
		),
		http.get(`/api/v1/hosts/${detail.id}/snapshots`, () =>
			HttpResponse.json({
				items,
				page: 1,
				page_size: 50,
				total: items.length,
			}),
		),
	);
}

async function openSnapshots(items: SnapshotItem[]) {
	mockHost(items);
	useSignedIn();
	renderApp(`/hosts/${detail.id}?tab=snapshots`);
	await screen.findByRole("heading", { level: 1, name: "web-1" });
	return screen.findByRole("table");
}

async function openMenu(item: string) {
	const user = userEvent.setup();
	await user.click(
		await screen.findByRole("button", { name: `Actions for snapshot ${item}` }),
	);
	return user;
}

describe("Host snapshots: suspicious", () => {
	it("host snapshots show suspicious badge with reasons", async () => {
		const table = await openSnapshots([
			makeSnapshot({ suspicious: automatic }),
			makeSnapshot({
				id: "bbbbbbbb00000000",
				suspicious: manual,
				incomplete: true,
			}),
			makeSnapshot({ id: "cccccccc00000000" }),
		]);
		const rows = within(table).getAllByRole("row").slice(1);

		const badge = within(rows[0])
			.getByText("Suspicious")
			.closest("[data-status]");
		expect(badge).toHaveAttribute("data-status", "suspicious");
		expect(badge?.querySelector("svg")).not.toBeNull();
		const label = badge?.getAttribute("aria-label") ?? "";
		expect(label).toContain("62% fewer files than before (12,480 to 4,742).");
		expect(label).toContain("5 GiB of new data, about 12.4 times the usual.");
		expect(within(rows[0]).getByText("Complete")).toBeInTheDocument();

		expect(
			within(rows[1]).getByText("Suspicious").closest("[data-status]"),
		).toHaveAttribute(
			"aria-label",
			"Suspicious. Marked by admin: Ransomware note found",
		);
		const order = Array.from(rows[1].querySelectorAll("[data-status]")).map(
			(el) => el.getAttribute("data-status"),
		);
		expect(order).toEqual(["suspicious", "warning"]);
		expect(within(rows[2]).queryByText("Suspicious")).toBeNull();
	});

	it("unverified snapshot shows the badge with its tooltip", async () => {
		const table = await openSnapshots([
			makeSnapshot({ verified_by: null }),
			makeSnapshot({ id: "bbbbbbbb00000000" }),
		]);
		const rows = within(table).getAllByRole("row").slice(1);

		const badge = within(rows[0])
			.getByText("Unverified")
			.closest("[data-status]");
		expect(badge).toHaveAttribute("data-status", "unverified");
		expect(badge?.getAttribute("aria-label")).toContain(
			"Not linked to a server job.",
		);
		expect(within(rows[1]).queryByText("Unverified")).toBeNull();
	});

	it("owner-signed snapshot shows the owner badge", async () => {
		const table = await openSnapshots([
			makeSnapshot({ verified_by: "owner" }),
			makeSnapshot({ id: "bbbbbbbb00000000" }),
		]);
		const rows = within(table).getAllByRole("row").slice(1);

		expect(
			within(rows[0])
				.getByText("Signed with password")
				.closest("[data-status]"),
		).toHaveAttribute("data-status", "owner-signed");
		expect(within(rows[0]).queryByText("Unverified")).toBeNull();
		expect(within(rows[1]).queryByText("Signed with password")).toBeNull();
	});

	it("policy deleted badge in source", async () => {
		const table = await openSnapshots([
			makeSnapshot({ policy_deleted: true }),
			makeSnapshot({ id: "bbbbbbbb00000000" }),
		]);
		const rows = within(table).getAllByRole("row").slice(1);
		expect(within(rows[0]).getByText("etc")).toBeInTheDocument();
		expect(within(rows[0]).queryByRole("link", { name: "etc" })).toBeNull();
		expect(
			within(rows[0]).getByText("Policy deleted").closest("[data-status]"),
		).toHaveAttribute("data-status", "policy-deleted");
		expect(within(rows[1]).queryByText("Policy deleted")).toBeNull();
	});

	it("mark suspicious flow", async () => {
		let body: unknown;
		await openSnapshots([makeSnapshot()]);
		server.use(
			http.put(`/api/v1/snapshots/${ID}/suspicious`, async ({ request }) => {
				body = await request.json();
				return HttpResponse.json(makeSnapshot({ suspicious: manual }));
			}),
		);
		const user = await openMenu(SHORT);
		expect(
			screen.queryByRole("menuitem", { name: "Clear suspicious mark" }),
		).toBeNull();
		await user.click(
			await screen.findByRole("menuitem", { name: "Mark as suspicious" }),
		);

		const dialog = await screen.findByRole("dialog", {
			name: `Mark snapshot ${SHORT} as suspicious`,
		});
		await user.type(
			within(dialog).getByRole("textbox", { name: "Reason" }),
			"  Encrypted files  ",
		);
		await user.click(
			within(dialog).getByRole("button", { name: "Mark as suspicious" }),
		);

		expect(
			await screen.findByText(`Snapshot ${SHORT} marked as suspicious.`),
		).toBeInTheDocument();
		expect(body).toEqual({ suspicious: true, reason: "Encrypted files" });
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
	});

	it("clear suspicious mark requires reason", async () => {
		let body: unknown;
		await openSnapshots([makeSnapshot({ suspicious: automatic })]);
		server.use(
			http.put(`/api/v1/snapshots/${ID}/suspicious`, async ({ request }) => {
				body = await request.json();
				return HttpResponse.json(makeSnapshot());
			}),
		);
		const user = await openMenu(SHORT);
		expect(
			screen.queryByRole("menuitem", { name: "Mark as suspicious" }),
		).toBeNull();
		await user.click(
			await screen.findByRole("menuitem", { name: "Clear suspicious mark" }),
		);

		const dialog = await screen.findByRole("dialog", {
			name: `Clear the suspicious mark on ${SHORT}?`,
		});
		expect(
			within(dialog).getByText(
				"The snapshot returns to normal retention. If your rules no longer keep it, the next backup of this host can forget it.",
			),
		).toBeInTheDocument();
		await user.click(
			within(dialog).getByRole("button", { name: "Clear mark" }),
		);
		expect(
			await within(dialog).findByText("Enter a reason."),
		).toBeInTheDocument();
		expect(body).toBeUndefined();

		await user.type(
			within(dialog).getByRole("textbox", { name: "Reason" }),
			"Backup of a re-encoded volume",
		);
		await user.click(
			within(dialog).getByRole("button", { name: "Clear mark" }),
		);
		expect(
			await screen.findByText(`Suspicious mark cleared on snapshot ${SHORT}.`),
		).toBeInTheDocument();
		expect(body).toEqual({
			suspicious: false,
			reason: "Backup of a re-encoded volume",
		});
	});

	it("mark suspicious error keeps dialog", async () => {
		await openSnapshots([makeSnapshot()]);
		server.use(
			http.put(`/api/v1/snapshots/${ID}/suspicious`, () =>
				apiError(502, "storage", "Storage is unreachable."),
			),
		);
		const user = await openMenu(SHORT);
		await user.click(
			await screen.findByRole("menuitem", { name: "Mark as suspicious" }),
		);
		const dialog = await screen.findByRole("dialog");
		await user.type(within(dialog).getByRole("textbox"), "odd");
		await user.click(
			within(dialog).getByRole("button", { name: "Mark as suspicious" }),
		);
		expect(
			await within(dialog).findByText(
				"Couldn't mark the snapshot as suspicious. Storage is unreachable. Try again.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("dialog")).toBeInTheDocument();
	});
});

describe("Host snapshots: delete", () => {
	async function openDelete() {
		const user = await openMenu(SHORT);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete snapshot" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: `Delete snapshot ${SHORT}?`,
		});
		return { user, dialog };
	}

	it("delete snapshot flow", async () => {
		let items = [makeSnapshot(), makeSnapshot({ id: "bbbbbbbb00000000" })];
		await openSnapshots(items);
		server.use(
			http.get(`/api/v1/hosts/${detail.id}/snapshots`, () =>
				HttpResponse.json({
					items,
					page: 1,
					page_size: 50,
					total: items.length,
				}),
			),
			http.delete(`/api/v1/snapshots/${ID}`, () => {
				items = items.slice(1);
				return HttpResponse.json({ id: ID, storage_kept: false });
			}),
		);
		const { user, dialog } = await openDelete();
		expect(
			within(dialog).getByText(
				"It is removed from the repository, from Restore and from lode. This cannot be undone. Storage space is freed later by garbage collection.",
			),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(
				within(dialog).getByRole("button", { name: "Keep snapshot" }),
			).toHaveFocus(),
		);

		await user.click(
			within(dialog).getByRole("button", { name: "Delete snapshot" }),
		);
		expect(
			await screen.findByText(`Snapshot ${SHORT} deleted.`),
		).toBeInTheDocument();
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		await waitFor(() =>
			expect(
				screen.queryByRole("button", {
					name: `Actions for snapshot ${SHORT}`,
				}),
			).toBeNull(),
		);
	});

	it("delete snapshot object lock warning", async () => {
		await openSnapshots([makeSnapshot()]);
		server.use(
			http.delete(`/api/v1/snapshots/${ID}`, () =>
				HttpResponse.json({
					id: ID,
					storage_kept: true,
					reason: "Object Lock",
				}),
			),
		);
		const { user, dialog } = await openDelete();
		await user.click(
			within(dialog).getByRole("button", { name: "Delete snapshot" }),
		);
		expect(
			await screen.findByText(
				`Snapshot ${SHORT} was removed from the catalog, but storage kept the file because of Object Lock. It stays out of Restore.`,
			),
		).toBeInTheDocument();
	});

	it("delete snapshot error keeps dialog", async () => {
		await openSnapshots([makeSnapshot()]);
		server.use(
			http.delete(`/api/v1/snapshots/${ID}`, () =>
				apiError(502, "storage", "Storage is unreachable."),
			),
		);
		const { user, dialog } = await openDelete();
		await user.click(
			within(dialog).getByRole("button", { name: "Delete snapshot" }),
		);
		expect(
			await screen.findByText(
				"Couldn't delete the snapshot. Storage is unreachable. Try again.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("alertdialog")).toBeInTheDocument();
	});
});

describe("Restore: suspicious", () => {
	beforeEach(() => {
		window.innerWidth = 1440;
	});
	afterEach(() => {
		window.innerWidth = 1024;
	});

	const HOST = "agent-web";
	const SNAP = "a".repeat(64);
	const OTHER = "b".repeat(64);
	const ORPHAN = "c".repeat(64);

	const restoreSnapshot = (
		id: string,
		over: Partial<RestoreSnapshot> = {},
	): RestoreSnapshot => ({
		...makeSnapshot({ id }),
		host_key: HOST,
		repository_id: "repo-1",
		time: Date.now() - 3_600_000,
		...over,
	});

	const restoreHost: RestoreHost = {
		key: HOST,
		host_id: HOST,
		hostname: "web-1",
		agent: true,
		status: "online",
		labels: [],
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 3,
		latest_snapshot_ms: Date.now(),
		unverified_count: 0,
	};

	function mockRestore(snapshots: RestoreSnapshot[]) {
		server.use(
			http.get("/api/v1/restore/hosts", () => HttpResponse.json([restoreHost])),
			http.get("/api/v1/restore/snapshots", () =>
				HttpResponse.json({
					items: snapshots,
					total: snapshots.length,
					next: null,
				}),
			),
			http.get("/api/v1/snapshots/:id", ({ params }) =>
				HttpResponse.json(snapshots.find((s) => s.id === params.id)),
			),
			http.get("/api/v1/snapshots/:id/entries", () =>
				HttpResponse.json({
					path: "",
					path_b64: "",
					entries: [
						{
							name: "file-001.log",
							name_b64: btoa("file-001.log"),
							kind: "file",
							size: 1536,
							mtime_ms: Date.now(),
							mode: 0o100644,
							uid: 1000,
							gid: 1000,
							link_target: null,
						},
					],
					total: 1,
					offset: 0,
				}),
			),
			http.post("/api/v1/snapshots/:id/summary", () =>
				HttpResponse.json({ files: 1, dirs: 0, bytes: 1536 }),
			),
		);
	}

	const suspiciousSnapshots = () => [
		restoreSnapshot(SNAP, { suspicious: automatic }),
		restoreSnapshot(OTHER, { incomplete: true, suspicious: manual }),
		restoreSnapshot(ORPHAN, { policy_deleted: true }),
	];

	function openRestore(snapshots: RestoreSnapshot[], selected = SNAP) {
		mockRestore(snapshots);
		useSignedIn();
		renderApp(`/restore?host=${HOST}&snapshot=${selected}`);
	}

	it("timeline shows suspicious and policy deleted", async () => {
		openRestore(suspiciousSnapshots());
		const stops = await screen.findAllByRole("button", { name: /^Snapshot / });
		const stop = (id: string) =>
			stops.find((el) =>
				el.getAttribute("aria-label")?.includes(id.slice(0, 8)),
			);

		const [first, second, orphan] = [SNAP, OTHER, ORPHAN].map(
			(id) => stop(id) as HTMLElement,
		);
		expect(first.querySelector('[data-status="suspicious"]')).not.toBeNull();
		const order = Array.from(second.querySelectorAll("[data-status]")).map(
			(el) => el.getAttribute("data-status"),
		);
		expect(order).toEqual(["suspicious", "warning"]);
		expect(orphan).toHaveTextContent("etc · policy deleted · cccccccc");
		expect(orphan.querySelector("[data-status]")).toBeNull();
		expect(first).not.toHaveTextContent("policy deleted");
	});

	it("snapshot header alert offers clear and delete", async () => {
		openRestore(suspiciousSnapshots());
		const title = await screen.findByText("This snapshot looks unusual.");
		const alert = title.closest("[data-slot=alert]") as HTMLElement;
		expect(
			within(alert).getByText("62% fewer files than before (12,480 to 4,742)."),
		).toBeInTheDocument();
		expect(
			within(alert).getByText(
				"Retention does not count it and does not forget it. Review it, then clear the mark or delete the snapshot.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Actions for snapshot aaaaaaaa" }),
		).toBeInTheDocument();

		const user = userEvent.setup();
		await user.click(
			within(alert).getByRole("button", { name: "Clear mark…" }),
		);
		expect(
			await screen.findByRole("dialog", {
				name: "Clear the suspicious mark on aaaaaaaa?",
			}),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Keep mark" }));
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

		await user.click(
			within(alert).getByRole("button", { name: "Delete snapshot…" }),
		);
		expect(
			await screen.findByRole("alertdialog", {
				name: "Delete snapshot aaaaaaaa?",
			}),
		).toBeInTheDocument();
	});

	it("snapshot header shows who marked it by hand", async () => {
		openRestore(suspiciousSnapshots(), OTHER);
		expect(
			await screen.findByText("Marked by admin: Ransomware note found"),
		).toBeInTheDocument();
		expect(
			screen.getAllByText("Incomplete", { selector: "[data-status] span" }),
		).toHaveLength(2);
	});

	it("restore dialog warns on suspicious", async () => {
		openRestore(suspiciousSnapshots());
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.click(screen.getByRole("button", { name: "Restore…" }));
		const dialog = await screen.findByRole("dialog");
		expect(
			within(dialog).getByText(
				"This snapshot is marked suspicious. Files in it may be encrypted or damaged.",
			),
		).toBeInTheDocument();
		expect(
			within(dialog).getByRole("button", { name: "Start restore" }),
		).toBeEnabled();
	});

	it("restore dialog stays quiet on a normal snapshot", async () => {
		openRestore(suspiciousSnapshots(), ORPHAN);
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("checkbox", { name: "Select file-001.log" }),
		);
		await user.click(screen.getByRole("button", { name: "Restore…" }));
		const dialog = await screen.findByRole("dialog");
		expect(within(dialog).queryByText(/marked suspicious/)).toBeNull();
	});
});
