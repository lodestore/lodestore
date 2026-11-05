import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import type { RestoreHost, RestoreSnapshot } from "@/api/gen/model";
import { encodeB64 } from "@/lib/names";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import {
	findGroup,
	findHandlers,
	findId,
	findSession,
	findSnapshotRef,
	findVersion,
	manySnapshots,
} from "@/test/find";
import { server } from "@/test/server";

const enc = (text: string) => encodeB64(new TextEncoder().encode(text));
const FILE = enc("/etc/nginx/nginx.conf");

const snapshot: RestoreSnapshot = {
	id: findId(2),
	hostname: "web-0",
	host_key: "agent-0",
	repository_id: "repo-1",
	time: Date.UTC(2026, 9, 21, 2, 0),
	incomplete: false,
	paths: ["/etc"],
	policy_name: "etc",
	policy_deleted: false,
	received_at: Date.UTC(2026, 9, 21, 2, 1),
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
const host: RestoreHost = {
	key: "agent-0",
	host_id: "agent-0",
	hostname: "web-0",
	agent: true,
	status: "online",
	labels: [],
	repository_id: "repo-1",
	repository_name: "main",
	snapshot_count: 2,
	latest_snapshot_ms: snapshot.time,
	unverified_count: 0,
};

function mockRestore() {
	server.use(
		http.get("/api/v1/restore/hosts", () => HttpResponse.json([host])),
		http.get("/api/v1/restore/snapshots", () =>
			HttpResponse.json({ items: [snapshot], total: 1, next: null }),
		),
		http.get("/api/v1/snapshots/:id", () => HttpResponse.json(snapshot)),
		http.get("/api/v1/snapshots/:id/entries", () =>
			HttpResponse.json({
				path: "/etc/nginx",
				path_b64: enc("/etc/nginx"),
				entries: [],
				total: 0,
				offset: 0,
			}),
		),
	);
}

function openFind(
	groups = [findGroup(0)],
	url = "/restore?tab=find&find=nginx.conf",
) {
	useSignedIn();
	findHandlers([findSession({ state: "done", groups })]);
	return renderApp(url);
}

afterEach(() => {
	for (const frame of document.querySelectorAll("iframe[hidden]")) {
		frame.remove();
	}
});

describe("Find file version actions", { timeout: 30_000 }, () => {
	it("shows the file in its snapshot with the file already selected", async () => {
		mockRestore();
		const { router } = openFind();
		const user = userEvent.setup();
		const [show] = await screen.findAllByRole("link", {
			name: /^Show \/etc\/nginx\/nginx\.conf in the/,
		});
		await user.click(show);

		await waitFor(() =>
			expect(router.state.location.search).toEqual({
				host: "agent-0",
				snapshot: findId(2),
				path: enc("/etc/nginx"),
				select: FILE,
			}),
		);
		expect(
			await screen.findByText("1 item selected", { selector: "[role=status]" }),
		).toBeInTheDocument();
		expect(screen.getByRole("tab", { name: "By host" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	});

	it("links each snapshot chip to its own snapshot", async () => {
		openFind();
		const chips = (
			await screen.findAllByRole("link", { name: /^Show / })
		).slice(1);
		expect(chips).toHaveLength(2);
		const ids = chips.map((chip) =>
			new URL(chip.getAttribute("href") ?? "", "http://x").searchParams.get(
				"snapshot",
			),
		);
		expect(ids).toEqual([findId(2), findId(1)]);
	});

	it("downloads the version through the archive dialog", async () => {
		mockRestore();
		let body: unknown;
		server.use(
			http.post("/api/v1/downloads", async ({ request }) => {
				body = await request.json();
				return HttpResponse.json(
					{
						id: "dl-1",
						url: "/api/v1/downloads/dl-1/archive",
						file_name: "a.tar.gz",
					},
					{ status: 201 },
				);
			}),
		);
		openFind();
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", {
				name: /^Download \/etc\/nginx\/nginx\.conf/,
			}),
		);

		const dialog = await screen.findByRole("dialog");
		expect(
			await within(dialog).findByRole("heading", { name: "Download 1 item" }),
		).toBeVisible();
		await user.click(
			await within(dialog).findByRole("button", { name: "Download" }),
		);
		await waitFor(() =>
			expect(body).toMatchObject({ snapshot_id: findId(2), items: [FILE] }),
		);
	});

	it.each([
		["Restore", /^Restore /],
		["Download", /^Download /],
	])("explains a forgotten snapshot instead of opening %s", async (_, name) => {
		server.use(
			http.get("/api/v1/restore/hosts", () => HttpResponse.json([host])),
			http.get("/api/v1/snapshots/:id", () =>
				apiError(404, "not_found", "no such snapshot"),
			),
		);
		openFind();
		const user = userEvent.setup();
		await user.click(await screen.findByRole("button", { name }));

		expect(
			await screen.findByText("This snapshot no longer exists."),
		).toBeVisible();
		expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
	});
});

describe("Find file version provenance", { timeout: 30_000 }, () => {
	it("marks a version that nothing signed as Unverified", async () => {
		openFind([
			findGroup(0, {
				versions: [
					findVersion(),
					findVersion({ content_id: "ab".repeat(16), verified_by: null }),
				],
			}),
		]);
		const badges = await screen.findAllByText("Unverified");
		expect(badges).toHaveLength(1);
		expect(badges[0].closest("[data-status=unverified]")).toBeInTheDocument();
	});

	it("marks a version signed with the repository password", async () => {
		openFind([
			findGroup(0, { versions: [findVersion({ verified_by: "owner" })] }),
		]);
		expect(await screen.findByText("Signed with password")).toBeVisible();
		expect(screen.queryByText("Unverified")).not.toBeInTheDocument();
	});
});

describe("Find file version history", { timeout: 30_000 }, () => {
	const showLinks = () => screen.getAllByRole("link", { name: /^Show / });

	it("expands every snapshot of a version in place", async () => {
		const day = 86_400_000;
		const everySnapshot = Array.from({ length: 9 }, (_, i) =>
			findSnapshotRef(9 - i, {
				time_ms: Date.UTC(2026, 9, 21, 2, 0) - i * day,
			}),
		);
		const crowded = (snapshots: typeof everySnapshot) =>
			findGroup(0, {
				versions: [findVersion({ snapshot_count: 9, snapshots })],
			});
		let requested = 0;
		server.use(
			http.get("/api/v1/find/:id/groups/:index", ({ params }) => {
				requested += 1;
				expect(params.index).toBe("0");
				return HttpResponse.json(crowded(everySnapshot));
			}),
		);
		openFind([crowded(manySnapshots(9))]);
		const user = userEvent.setup();

		await screen.findByText("Present in 9 snapshots");
		expect(showLinks()).toHaveLength(7);
		await user.click(screen.getByRole("button", { name: "All 9 snapshots" }));

		await waitFor(() => expect(showLinks()).toHaveLength(10));
		expect(requested).toBe(1);
		expect(
			screen.queryByRole("button", { name: "All 9 snapshots" }),
		).not.toBeInTheDocument();
	});

	it("marks a file that is gone from later snapshots", async () => {
		const gone = {
			last_ms: Date.UTC(2026, 9, 21, 12),
			next_ms: Date.UTC(2026, 9, 24, 12),
		};
		openFind([findGroup(0, { gone })]);

		const list = within(
			await screen.findByRole("navigation", { name: "Matching files" }),
		);
		const item = await list.findByRole("link");
		expect(within(item).getByText(/^Last seen Oct \d+$/)).toHaveClass(
			"text-warning",
		);
		expect(
			await screen.findByText("Not in snapshots after Oct 21."),
		).toBeVisible();
		expect(
			screen.getByText(/deleted or moved before the Oct 24 backup/u),
		).toBeVisible();
	});

	it("shows both times when the last and the next snapshot fall on one day", async () => {
		const gone = {
			last_ms: Date.UTC(2026, 9, 21, 10),
			next_ms: Date.UTC(2026, 9, 21, 14),
		};
		openFind([findGroup(0, { gone })]);

		expect(
			await screen.findByText(/Not in snapshots after Oct 21, \d\d:\d\d\./),
		).toBeVisible();
		expect(
			screen.getByText(/before the Oct 21, \d\d:\d\d backup/u),
		).toBeVisible();
	});

	it("separates the version id, size and mtime with dots", async () => {
		openFind([findGroup(0)]);

		const id = await screen.findByText("c41e…9a07");
		expect(id.parentElement).toHaveTextContent(
			/^c41e…9a07 · \S+ \S+ · modified Oct 15, \d\d:\d\d$/u,
		);
	});

	it("tells that a host without an agent can still download or restore elsewhere", async () => {
		openFind([findGroup(0, { agent: false })]);
		expect(
			await screen.findByText(/No agent is connected for .*web-0/u),
		).toBeVisible();
	});
});
