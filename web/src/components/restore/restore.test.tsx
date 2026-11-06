import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
	RestoreHost,
	RestoreSnapshot,
	RestoreSnapshotPage,
} from "@/api/gen/model";
import { formatDay } from "@/lib/format";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;
const iso = (text: string) => `\u2068${text}\u2069`;
const noon = (daysAgo: number) => {
	const date = new Date();
	date.setHours(12, 0, 0, 0);
	return date.getTime() - daysAgo * DAY;
};

const id = (n: number) => `${String(n).padStart(8, "a")}${"0".repeat(56)}`;
const short = (n: number) => id(n).slice(0, 8);

function snapshot(
	n: number,
	time: number,
	host: string,
	over: Partial<RestoreSnapshot> = {},
): RestoreSnapshot {
	return {
		id: id(n),
		hostname: "web-1",
		host_key: host,
		repository_id: "repo-1",
		time,
		incomplete: false,
		paths: ["/etc"],
		policy_name: "etc-and-home",
		policy_deleted: false,
		received_at: time,
		verified_by: "server",
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
		...over,
	};
}

const WEB = "agent-web";
const DB = "agent-db";

const hosts: RestoreHost[] = [
	{
		key: DB,
		host_id: DB,
		hostname: "db-1",
		agent: true,
		status: "offline",
		labels: [{ key: "env", value: "prod" }],
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 1,
		latest_snapshot_ms: noon(30),
		unverified_count: 0,
		restore_roots: [],
	},
	{
		key: WEB,
		host_id: WEB,
		hostname: "web-1",
		agent: true,
		status: "online",
		labels: [],
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 5,
		latest_snapshot_ms: noon(0),
		unverified_count: 0,
		restore_roots: [],
	},
	{
		key: "agentless:old-nas",
		host_id: null,
		hostname: "old-nas",
		agent: false,
		status: null,
		labels: [],
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 2,
		latest_snapshot_ms: noon(10),
		unverified_count: 0,
		restore_roots: [],
	},
];

const webSnapshots = [
	snapshot(1, noon(0), WEB),
	snapshot(2, noon(0) - 3 * 3_600_000, WEB, { incomplete: true }),
	snapshot(3, noon(1), WEB, { policy_name: null, stdin_name: "pg_dump" }),
	snapshot(4, noon(40), WEB),
	snapshot(5, noon(41), WEB),
];

function mockRestore(
	options: {
		hosts?: RestoreHost[];
		bySnapshot?: Record<string, RestoreSnapshot>;
		seen?: URL[];
		web?: RestoreSnapshot[];
		latestVerified?: string | null;
	} = {},
) {
	const list = options.hosts ?? hosts;
	const web = options.web ?? webSnapshots;
	const latestVerified =
		options.latestVerified === undefined ? web[0].id : options.latestVerified;
	server.use(
		http.get("/api/v1/restore/hosts", () => HttpResponse.json(list)),
		http.get("/api/v1/restore/snapshots", ({ request }) => {
			const url = new URL(request.url);
			options.seen?.push(url);
			const host = url.searchParams.get("host");
			if (host !== WEB) {
				const own = Object.values(options.bySnapshot ?? {}).filter(
					(s) => s.host_key === host,
				);
				return HttpResponse.json<RestoreSnapshotPage>({
					items: own,
					total: own.length,
					next: null,
					latest_verified_id: own[0]?.id ?? null,
				});
			}
			const second = url.searchParams.get("before") === "cursor-1";
			return HttpResponse.json<RestoreSnapshotPage>({
				items: second ? web.slice(3) : web.slice(0, 3),
				total: web.length,
				next: second ? null : "cursor-1",
				latest_verified_id: latestVerified,
			});
		}),
		http.get("/api/v1/snapshots/:id", ({ params }) => {
			const found = [...web, ...Object.values(options.bySnapshot ?? {})].find(
				(s) => s.id === params.id,
			);
			return found
				? HttpResponse.json(found)
				: apiError(404, "not_found", "snapshot not found");
		}),
	);
}

async function openRestore(path = "/restore") {
	useSignedIn();
	const app = renderApp(path);
	await screen.findByRole("heading", { level: 1, name: "Restore" });
	return app;
}

const stops = () =>
	screen.queryAllByRole("button", { name: /^Snapshot [0-9a-f]{8} from / });

beforeEach(() => {
	window.innerWidth = 1440;
});
afterEach(() => {
	window.innerWidth = 1024;
});

describe("Restore timeline", () => {
	it("ignores a snapshot id that is not a snapshot hash", async () => {
		mockRestore();
		const { router } = await openRestore(
			`/restore?host=${WEB}&snapshot=${encodeURIComponent("../../x?")}`,
		);

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				host: WEB,
				snapshot: id(1),
			}),
		);
	});

	it("opens the host with the newest snapshot and selects it", async () => {
		mockRestore();
		const { router } = await openRestore();

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				host: WEB,
				snapshot: id(1),
			}),
		);
		expect(
			await screen.findByRole("heading", { name: /^Snapshot aaaaaaa1/ }),
		).toBeInTheDocument();
		const selected = (
			await screen.findAllByRole("button", {
				name: new RegExp(`^Snapshot ${short(1)} from `),
			})
		)[0];
		expect(selected).toHaveAttribute("aria-current", "true");
		expect(selected).toHaveAccessibleDescription(/Selected/);
		expect(stops().filter((s) => s.hasAttribute("aria-current"))).toHaveLength(
			1,
		);
	});

	it("opens the latest signed snapshot, not the newest listed", async () => {
		mockRestore({
			web: [
				snapshot(1, noon(0), WEB, { verified_by: null }),
				...webSnapshots.slice(1),
			],
			latestVerified: id(2),
		});
		const { router } = await openRestore();

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				host: WEB,
				snapshot: id(2),
			}),
		);
		const unsigned = (
			await screen.findAllByRole("button", {
				name: new RegExp(`^Snapshot ${short(1)} from `),
			})
		)[0];
		expect(unsigned).not.toHaveAttribute("aria-current");
		expect(unsigned).toHaveAccessibleDescription(/Unverified/);
	});

	it("does not call a snapshot the server is still checking unsigned", async () => {
		mockRestore({
			web: [
				snapshot(1, noon(0), WEB, { verified_by: null, job_id: "job-1" }),
				...webSnapshots.slice(1),
			],
			latestVerified: id(2),
		});
		await openRestore(`/restore?host=${WEB}&snapshot=${id(1)}`);

		expect(
			await screen.findByRole("heading", {
				name: new RegExp(`Snapshot ${short(1)}`),
			}),
		).toBeInTheDocument();
		expect(screen.getAllByText("Verifying").length).toBeGreaterThan(0);
		expect(
			screen.queryByText("Nothing signed this snapshot"),
		).not.toBeInTheDocument();
	});

	it("host with only unsigned snapshots has no default snapshot", async () => {
		mockRestore({
			hosts: hosts.map((host) =>
				host.key === WEB
					? { ...host, latest_snapshot_ms: null, unverified_count: 2 }
					: host,
			),
			web: [snapshot(1, noon(0), WEB, { verified_by: null })],
			latestVerified: null,
		});
		const user = userEvent.setup();
		const { router } = await openRestore(`/restore?host=${WEB}`);

		expect(
			await screen.findByText(/Nothing signed any of this host's snapshots/),
		).toBeInTheDocument();
		expect(router.state.location.search.snapshot).toBeUndefined();
		await user.click(screen.getByRole("combobox", { name: /^Host: web-1/ }));
		const option = await screen.findByRole("option", { name: /web-1/ });
		expect(within(option).getByText("Unverified 2")).toBeInTheDocument();
		expect(within(option).getByText("No signed snapshots")).toBeInTheDocument();
	});

	it("groups snapshots by day, newest first, and flags incomplete ones", async () => {
		mockRestore();
		await openRestore();
		await screen.findAllByRole("button", {
			name: /^Snapshot [0-9a-f]{8} from /,
		});

		const headings = screen
			.getAllByRole("heading", { level: 3 })
			.map((h) => h.textContent);
		expect(headings).toEqual(["Today", "Yesterday"]);
		const today = screen
			.getByRole("heading", { name: "Today" })
			.closest("section") as HTMLElement;
		expect(within(today).getAllByRole("button")).toHaveLength(2);
		expect(within(today).getByText("Incomplete")).toBeInTheDocument();
		const incomplete = within(today)
			.getByText("Incomplete")
			.closest("button") as HTMLElement;
		expect(incomplete).toHaveAccessibleDescription(/Incomplete/);
		expect(within(today).getAllByText("12:00").length).toBeGreaterThan(0);
		const yesterday = screen
			.getByRole("heading", { name: "Yesterday" })
			.closest("section") as HTMLElement;
		expect(within(yesterday).getByText(/CLI · pg_dump/)).toBeInTheDocument();
		expect(formatDay(noon(40))).toMatch(/^\d{1,2} \w{3} \d{4}$/);
	});

	it("groups by receive day when manifest times are out of order", async () => {
		mockRestore({
			web: [
				snapshot(1, noon(5), WEB, { received_at: noon(0) }),
				snapshot(2, noon(0), WEB, { received_at: noon(1) }),
				snapshot(3, noon(7), WEB, { received_at: noon(0) }),
			],
			latestVerified: id(1),
		});
		const errors = vi.spyOn(console, "error").mockImplementation(() => {});
		await openRestore();
		await screen.findAllByRole("button", {
			name: /^Snapshot [0-9a-f]{8} from /,
		});

		expect(
			screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent),
		).toEqual(["Today", "Yesterday"]);
		const today = screen
			.getByRole("heading", { name: "Today" })
			.closest("section") as HTMLElement;
		expect(within(today).getAllByRole("button")).toHaveLength(2);
		expect(errors).not.toHaveBeenCalledWith(
			expect.stringContaining("same key"),
			expect.anything(),
			expect.anything(),
		);
		errors.mockRestore();
	});

	it("moves focus with the arrow keys and selects only on Enter", async () => {
		mockRestore();
		const user = userEvent.setup();
		const { router } = await openRestore();
		await waitFor(() =>
			expect(router.state.location.search.snapshot).toBe(id(1)),
		);
		await screen.findAllByRole("button", {
			name: /^Snapshot [0-9a-f]{8} from /,
		});

		const [first, second, third] = stops();
		expect(first).toHaveAttribute("tabindex", "0");
		expect(second).toHaveAttribute("tabindex", "-1");

		first.focus();
		await user.keyboard("{ArrowDown}");
		await waitFor(() => expect(second).toHaveFocus());
		expect(router.state.location.search.snapshot).toBe(id(1));
		await user.keyboard("{End}");
		await waitFor(() => expect(stops().at(-1)).toHaveFocus());
		await user.keyboard("{Home}");
		await waitFor(() => expect(first).toHaveFocus());
		await user.keyboard("{ArrowDown}{ArrowDown}");
		await waitFor(() => expect(third).toHaveFocus());

		await user.keyboard("{Enter}");
		await waitFor(() =>
			expect(router.state.location.search.snapshot).toBe(id(3)),
		);
		await waitFor(() =>
			expect(
				screen.getAllByRole("button", {
					name: new RegExp(`^Snapshot ${short(3)} from `),
				})[0],
			).toHaveAttribute("aria-current", "true"),
		);
	});

	it("loads older snapshots on demand and updates the counter", async () => {
		const seen: URL[] = [];
		mockRestore({ seen });
		const user = userEvent.setup();
		await openRestore();

		expect(await screen.findByText("Showing 3 of 5 snapshots")).toBeVisible();
		expect(screen.getByRole("heading", { name: "Snapshots 5" })).toBeVisible();
		await user.click(
			screen.getByRole("button", { name: "Show older snapshots" }),
		);
		expect(await screen.findByText("Showing 5 of 5 snapshots")).toBeVisible();
		expect(stops()).toHaveLength(5);
		expect(
			screen.queryByRole("button", { name: "Show older snapshots" }),
		).not.toBeInTheDocument();
		expect(
			seen.some((url) => url.searchParams.get("before") === "cursor-1"),
		).toBe(true);
	});
});

const nasSnapshot = snapshot(7, noon(10), "agentless:old-nas", {
	hostname: "old-nas",
});

describe("HostPicker", () => {
	it("groups hosts, filters by name and label, and returns focus on Escape", async () => {
		mockRestore({ bySnapshot: { nas: nasSnapshot } });
		const user = userEvent.setup();
		await openRestore();
		const trigger = await screen.findByRole("combobox", {
			name: "Host: web-1",
		});
		expect(trigger).toHaveAttribute("aria-expanded", "false");

		await user.click(trigger);
		expect(trigger).toHaveAttribute("aria-expanded", "true");
		const listbox = await screen.findByRole("listbox");
		expect(within(listbox).getByText("Hosts")).toBeInTheDocument();
		expect(
			within(listbox).getByText("Hosts without an agent"),
		).toBeInTheDocument();
		expect(
			within(listbox).getByText(/not enrolled on this server/),
		).toBeInTheDocument();
		const options = within(listbox).getAllByRole("option");
		expect(options.map((o) => o.textContent)).toEqual([
			expect.stringMatching(/^web-1\s*Online/),
			expect.stringMatching(/^db-1\s*env=prod\s*Offline/),
			expect.stringMatching(/^old-nas\s*No agent/),
		]);

		await user.keyboard("env=prod");
		expect(within(listbox).getAllByRole("option")).toHaveLength(1);
		await user.clear(screen.getByPlaceholderText("Search hosts"));
		await user.keyboard("zzz");
		expect(await screen.findByText('No host matches "zzz".')).toBeVisible();

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("listbox")).toBeNull());
		await waitFor(() => expect(trigger).toHaveFocus());
	});

	it("switches to a host without an agent and explains the limits", async () => {
		mockRestore({ bySnapshot: { nas: nasSnapshot } });
		const user = userEvent.setup();
		const { router } = await openRestore();
		await user.click(await screen.findByRole("combobox"));
		await user.click(await screen.findByRole("option", { name: /old-nas/ }));

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				host: "agentless:old-nas",
				snapshot: id(7),
			}),
		);
		expect(
			await screen.findByText(
				new RegExp(`^No agent is connected for ${iso("old-nas")}\\.`),
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("combobox", { name: "Host: old-nas" }),
		).toBeInTheDocument();
	});
});

describe("Restore states", () => {
	it("offers to create a policy when the catalog is empty", async () => {
		mockRestore({ hosts: [] });
		await openRestore();
		expect(await screen.findByText("Nothing to restore yet")).toBeVisible();
		expect(screen.getByRole("link", { name: "Create policy" })).toHaveAttribute(
			"href",
			"/policies/new",
		);
	});

	it("says so when the chosen host has no snapshots", async () => {
		mockRestore({
			hosts: hosts.map((h) => (h.key === DB ? { ...h, snapshot_count: 0 } : h)),
		});
		await openRestore(`/restore?host=${DB}`);
		expect(
			await screen.findByText(`No snapshots for ${iso("db-1")} yet`),
		).toBeVisible();
		expect(stops()).toHaveLength(0);
	});

	it("handles a snapshot that no longer exists", async () => {
		mockRestore();
		const user = userEvent.setup();
		const { router } = await openRestore(
			`/restore?host=${WEB}&snapshot=${"f".repeat(64)}`,
		);
		expect(
			await screen.findByText("This snapshot no longer exists."),
		).toBeVisible();
		await user.click(screen.getByRole("button", { name: "Back to snapshots" }));
		await waitFor(() =>
			expect(router.state.location.search.snapshot).toBe(id(1)),
		);
		expect(screen.queryByText("This snapshot no longer exists.")).toBeNull();
	});

	it("retries a failed snapshot list", async () => {
		mockRestore();
		let calls = 0;
		server.use(
			http.get("/api/v1/restore/snapshots", () => {
				calls += 1;
				if (calls === 1) return apiError(500, "internal", "boom");
			}),
		);
		const user = userEvent.setup();
		await openRestore();
		expect(
			await screen.findByText(/Couldn't load snapshots\./),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Retry loading" }));
		expect(await screen.findByText("Showing 3 of 5 snapshots")).toBeVisible();
		expect(calls).toBeGreaterThan(1);
	});

	it("says when the hosts cannot be loaded", async () => {
		mockRestore();
		server.use(
			http.get("/api/v1/restore/hosts", () =>
				apiError(500, "internal", "boom"),
			),
		);
		await openRestore();
		expect(
			await screen.findByText(
				"Couldn't load hosts. Check that the server is reachable, then retry.",
			),
		).toBeVisible();
	});

	it("retries a snapshot that failed to open", async () => {
		mockRestore();
		let calls = 0;
		server.use(
			http.get("/api/v1/snapshots/:id", () => {
				calls += 1;
				if (calls === 1) return apiError(500, "internal", "storage offline");
			}),
		);
		const user = userEvent.setup();
		await openRestore(`/restore?host=${WEB}&snapshot=${id(2)}`);
		expect(
			await screen.findByText(
				/Couldn't open this snapshot\. Storage offline\./,
			),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Retry loading" }));
		expect(
			await screen.findByRole("heading", { name: /^Snapshot aaaaaaa2/ }),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"Some files could not be read when this snapshot was made.",
			),
		).toBeVisible();
	});

	it("folds the timeline into a collapsible below 1280 px", async () => {
		window.innerWidth = 1024;
		mockRestore();
		const user = userEvent.setup();
		const { router } = await openRestore();
		await waitFor(() =>
			expect(router.state.location.search.snapshot).toBe(id(1)),
		);
		const change = await screen.findByRole("button", {
			name: "Change snapshot",
		});
		expect(screen.getByText(/^Snapshot .+ · aaaaaaa1$/)).toBeVisible();
		expect(stops()).toHaveLength(0);

		await user.click(change);
		await waitFor(() => expect(stops()).toHaveLength(3));
		await user.click(stops()[2]);
		await waitFor(() =>
			expect(router.state.location.search.snapshot).toBe(id(3)),
		);
		await waitFor(() => expect(stops()).toHaveLength(0));
		expect(screen.getByText(/^Snapshot .+ · aaaaaaa3$/)).toBeVisible();
	});
});
