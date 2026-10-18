import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	HostDetail,
	HostPolicy,
	JobSummary,
	SnapshotItem,
} from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};

function makeDetail(name: string, over: Partial<HostDetail> = {}): HostDetail {
	return {
		id: `${name.replace(/\W/g, "").padEnd(8, "0")}-0000-4000-8000-000000000000`,
		hostname: name,
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
		uptime_s: 3 * 86_400 + 4 * 3_600,
		mounts: [],
		...over,
	};
}

const jobsHostIdParams: Array<string | null> = [];

type Mocks = {
	policies?: HostPolicy[];
	jobs?: JobSummary[];
	snapshots?: SnapshotItem[];
};

function makeSnapshot(over: Partial<SnapshotItem> = {}): SnapshotItem {
	return {
		id: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
		hostname: "web-1",
		time: Date.now() - 7_200_000,
		paths: ["/etc"],
		tags: [],
		policy_name: "etc",
		policy_deleted: false,
		received_at: Date.now() - 7_100_000,
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

function makeJob(host: HostDetail, over: Partial<JobSummary> = {}): JobSummary {
	return {
		id: "job-1",
		kind: "backup",
		host_id: host.id,
		hostname: host.hostname,
		policy_name: "etc",
		policy_exists: true,
		source: "manual",
		origin: "manual",
		snapshot_suspicious: false,
		state: "succeeded",
		queued_at: Date.now() - 3_600_000,
		started_at: Date.now() - 3_500_000,
		finished_at: Date.now() - 3_400_000,
		host_online: true,
		incomplete: false,
		warnings_total: 0,
		errors_total: 0,
		stats: {
			bytes_total: 2048,
			files_new: 2,
			files_changed: 0,
			files_unchanged: 1,
		},
		...over,
	};
}

function mockDetail(detail: HostDetail, mocks: Mocks = {}) {
	server.use(
		http.get(`/api/v1/hosts/${detail.id}`, () => HttpResponse.json(detail)),
		http.get(`/api/v1/hosts/${detail.id}/policies`, () =>
			HttpResponse.json(mocks.policies ?? []),
		),
		http.get(`/api/v1/hosts/${detail.id}/snapshots`, ({ request }) => {
			const all = mocks.snapshots ?? [];
			const page = Number(new URL(request.url).searchParams.get("page") ?? 1);
			return HttpResponse.json({
				items: all.slice((page - 1) * 50, page * 50),
				page,
				page_size: 50,
				total: all.length,
			});
		}),
		http.get("/api/v1/jobs", ({ request }) => {
			const hostId = new URL(request.url).searchParams.get("host_id");
			jobsHostIdParams.push(hostId);
			const items = (mocks.jobs ?? []).filter(
				(job) => hostId === null || job.host_id === hostId,
			);
			return HttpResponse.json({
				items,
				page: 1,
				page_size: 50,
				total: items.length,
			});
		}),
	);
}

async function openHost(detail: HostDetail, search = "", mocks: Mocks = {}) {
	mockDetail(detail, mocks);
	useSignedIn();
	const view = renderApp(`/hosts/${detail.id}${search}`);
	await screen.findByRole(
		"heading",
		{ level: 1, name: detail.hostname },
		{ timeout: 5000 },
	);
	return view;
}

describe("Host page", () => {
	it("shows the host name, status, last seen and breadcrumb", async () => {
		const host = makeDetail("web-1");
		await openHost(host);

		const heading = screen.getByRole("heading", { level: 1 });
		expect(heading).toHaveTextContent("web-1");
		expect(heading).toHaveAttribute("data-page-title");
		expect(screen.getAllByText("Online").length).toBeGreaterThan(0);
		expect(screen.getByText(/^Last seen /)).toBeInTheDocument();
		const crumbs = screen.getByRole("navigation", { name: "breadcrumb" });
		expect(within(crumbs).getByRole("link", { name: "Hosts" })).toHaveAttribute(
			"href",
			"/hosts",
		);
		expect(
			screen.getByRole("button", { name: "Edit labels" }),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Revoke host" }),
		).toBeInTheDocument();
	});

	it("links the repository to its settings page", async () => {
		await openHost(makeDetail("web-1"));
		expect(await screen.findByRole("link", { name: "main" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1",
		);
	});

	it("limits filesystems to 20 rows and expands on request", async () => {
		const host = makeDetail("web-1", {
			mounts: Array.from({ length: 60 }, (_, i) => ({
				path: `/mnt/fs${String(i).padStart(2, "0")}`,
				fs_type: "ext4",
				total_bytes: 100 * 1024 ** 3,
				free_bytes: (100 - i) * 1024 ** 3,
			})),
		});
		await openHost(host);

		const table = await screen.findByRole("table");
		expect(within(table).getAllByRole("row")).toHaveLength(21);
		expect(within(table).getAllByRole("row")[1]).toHaveTextContent("/mnt/fs59");
		expect(within(table).getAllByRole("row")[1]).toHaveTextContent("59%");

		await userEvent.click(
			screen.getByRole("button", { name: "Show all 60 filesystems" }),
		);
		expect(within(table).getAllByRole("row")).toHaveLength(61);
		expect(screen.queryByRole("button", { name: /Show all/ })).toBeNull();
	});

	it("shows a dash for missing facts and hides empty cards", async () => {
		await openHost(makeDetail("fresh-1", { kernel: null, uptime_s: null }));

		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		expect(screen.getAllByText("—").length).toBeGreaterThanOrEqual(3);
		expect(screen.queryByRole("heading", { name: "Storage" })).toBeNull();
		expect(screen.queryByRole("heading", { name: "Labels" })).toBeNull();
	});

	it("reports a load failure and retries", async () => {
		const host = makeDetail("web-1");
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () =>
				HttpResponse.json({ code: "x", message: "boom" }, { status: 500 }),
			),
		);
		useSignedIn();
		renderApp(`/hosts/${host.id}`);
		expect(
			await screen.findByText(/Couldn't load this host\./),
		).toBeInTheDocument();

		mockDetail(host);
		await userEvent.click(
			screen.getByRole("button", { name: "Retry loading" }),
		);
		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		await waitFor(() =>
			expect(screen.queryByText(/Couldn't load this host\./)).toBeNull(),
		);
	});

	it("shows all labels on the Labels card", async () => {
		await openHost(
			makeDetail("web-1", {
				labels: [
					{ key: "env", value: "prod" },
					{ key: "team", value: "core" },
				],
			}),
		);
		expect(
			await screen.findByRole("heading", { name: "Labels" }),
		).toBeInTheDocument();
		expect(screen.getByText("env=prod")).toBeInTheDocument();
		expect(screen.getByText("team=core")).toBeInTheDocument();
	});

	it("renders a non-ASCII host name and label value in full", async () => {
		await openHost(
			makeDetail("сервер-бэкапов.пример", {
				labels: [{ key: "team", value: "Платформа" }],
			}),
		);
		expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
			"сервер-бэкапов.пример",
		);
		expect(await screen.findByText("team=Платформа")).toBeInTheDocument();
	});

	it("shows an unknown uptime as a dash when the agent reports zero", async () => {
		await openHost(makeDetail("mac-1", { uptime_s: 0 }));
		const label = await screen.findByText("Uptime");
		expect(label.nextElementSibling).toHaveTextContent("—");
	});

	it("falls back to Overview for an unknown tab", async () => {
		await openHost(makeDetail("web-1"), "?tab=bogus");
		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	});

	it("keeps the tab in the search parameters", async () => {
		const { router } = await openHost(makeDetail("web-1"));
		await userEvent.click(screen.getByRole("tab", { name: "Jobs" }));
		await waitFor(() =>
			expect(router.state.location.search).toEqual({ tab: "jobs" }),
		);
		expect(
			await screen.findByText("No jobs on this host yet"),
		).toBeInTheDocument();
	});

	it("shows a revoked banner and no actions for a revoked host", async () => {
		await openHost(makeDetail("old-1", { status: "revoked" }));
		expect(screen.getByText(/This host is revoked\./)).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Edit labels" })).toBeNull();
		expect(screen.queryByRole("button", { name: "Revoke host" })).toBeNull();
		expect(screen.queryByRole("button", { name: /Run backup/ })).toBeNull();
		expect(screen.getByRole("tab", { name: "Snapshots" })).toBeInTheDocument();
	});

	it("shows Host not found with a way back for an unknown id", async () => {
		server.use(
			http.get("/api/v1/hosts/missing", () =>
				HttpResponse.json(
					{ code: "not_found", message: "no" },
					{ status: 404 },
				),
			),
		);
		useSignedIn();
		renderApp("/hosts/missing");
		expect(
			await screen.findByRole("heading", { level: 1, name: "Host not found" }),
		).toBeInTheDocument();
		expect(screen.getByRole("link", { name: "Back to Hosts" })).toHaveAttribute(
			"href",
			"/hosts",
		);
		expect(screen.queryByText(/Couldn't load this host/)).toBeNull();
	});

	it("shows skeletons while the host loads", async () => {
		const host = makeDetail("web-1");
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, async () => {
				await new Promise((resolve) => setTimeout(resolve, 200));
				return HttpResponse.json(host);
			}),
		);
		useSignedIn();
		renderApp(`/hosts/${host.id}`);
		await waitFor(() =>
			expect(document.querySelector("[data-slot=skeleton]")).not.toBeNull(),
		);
		expect(await screen.findByRole("heading", { level: 1 })).toHaveTextContent(
			"web-1",
		);
	});

	it("stays on the page in the revoked state after revoking", async () => {
		const host = makeDetail("web-1");
		let revoked = false;
		server.use(
			http.get(`/api/v1/hosts/${host.id}/policies`, () =>
				HttpResponse.json([]),
			),
			http.get(`/api/v1/hosts/${host.id}`, () =>
				HttpResponse.json({ ...host, status: revoked ? "revoked" : "online" }),
			),
			http.post(`/api/v1/hosts/${host.id}/revoke`, () => {
				revoked = true;
				return new HttpResponse(null, { status: 204 });
			}),
		);
		useSignedIn();
		const { router } = renderApp(`/hosts/${host.id}`);
		await userEvent.click(
			await screen.findByRole("button", { name: "Revoke host" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Revoke host" }),
		);
		expect(
			await screen.findByText(/This host is revoked\./),
		).toBeInTheDocument();
		expect(router.state.location.pathname).toBe(`/hosts/${host.id}`);
		expect(
			(await screen.findAllByText('Host "web-1" revoked.')).length,
		).toBeGreaterThan(0);
	});

	describe("Run backup", () => {
		const policy = (name: string): HostPolicy => ({ name, last_job: null });
		const created = (host: HostDetail, id: string, isNew = true) => ({
			created: isNew,
			host_id: host.id,
			hostname: host.hostname,
			id,
		});

		it("starts the only policy at once and offers View job", async () => {
			const host = makeDetail("web-1");
			let body: unknown;
			server.use(
				http.post(`/api/v1/hosts/${host.id}/run`, async ({ request }) => {
					body = await request.json();
					return HttpResponse.json({ jobs: [created(host, "job-9")] });
				}),
			);
			await openHost(host, "", { policies: [policy("etc")] });

			await userEvent.click(
				await screen.findByRole("button", { name: "Run backup" }),
			);
			expect(
				(await screen.findAllByText("Backup queued for web-1.")).length,
			).toBeGreaterThan(0);
			expect(body).toEqual({ policies: ["etc"] });
			expect(
				screen.getByRole("button", { name: "View job" }),
			).toBeInTheDocument();
		});

		it("says when the job joined one already waiting", async () => {
			const host = makeDetail("web-1");
			server.use(
				http.post(`/api/v1/hosts/${host.id}/run`, () =>
					HttpResponse.json({ jobs: [created(host, "job-3", false)] }),
				),
			);
			await openHost(host, "", { policies: [policy("etc")] });
			await userEvent.click(
				await screen.findByRole("button", { name: "Run backup" }),
			);
			expect(
				(
					await screen.findAllByText(
						"Already queued for web-1. Showing the existing job.",
					)
				).length,
			).toBeGreaterThan(0);
		});

		it("offers a menu with every policy and All policies for several", async () => {
			const host = makeDetail("web-1");
			let body: unknown;
			server.use(
				http.post(`/api/v1/hosts/${host.id}/run`, async ({ request }) => {
					body = await request.json();
					return HttpResponse.json({
						jobs: [created(host, "a"), created(host, "b", false)],
					});
				}),
			);
			await openHost(host, "", { policies: [policy("etc"), policy("db")] });

			await userEvent.click(
				await screen.findByRole("button", { name: "Run backup" }),
			);
			const items = await screen.findAllByRole("menuitem");
			expect(items.map((item) => item.textContent)).toEqual([
				"etc",
				"db",
				"All policies (2)",
			]);
			await userEvent.click(items[2]);
			expect(
				(
					await screen.findAllByText(
						"2 jobs queued. 1 already queued or running.",
					)
				).length,
			).toBeGreaterThan(0);
			expect(body).toEqual({ policies: null });
			expect(
				screen.getByRole("button", { name: "View jobs" }),
			).toBeInTheDocument();
		});

		it("reports a failed start", async () => {
			const host = makeDetail("web-1");
			server.use(
				http.post(`/api/v1/hosts/${host.id}/run`, () =>
					HttpResponse.json(
						{ code: "x", message: "Policy gone" },
						{ status: 422 },
					),
				),
			);
			await openHost(host, "", { policies: [policy("etc")] });
			await userEvent.click(
				await screen.findByRole("button", { name: "Run backup" }),
			);
			expect(
				(await screen.findAllByText(/Couldn't start the backup\./)).length,
			).toBeGreaterThan(0);
		});

		it("has no button without policies and explains it on the Policies card", async () => {
			await openHost(makeDetail("web-1"));
			const card = (
				await screen.findByRole("heading", { name: "Policies" })
			).closest("[data-slot=card]") as HTMLElement;
			expect(
				await within(card).findByText(/^No policy applies to this host\./),
			).toBeInTheDocument();
			expect(
				within(card).getByRole("link", { name: "Create policy" }),
			).toHaveAttribute("href", "/policies/new");
			expect(screen.queryByRole("button", { name: /Run backup/ })).toBeNull();
		});

		it("lists policies with the status and time of the last job", async () => {
			await openHost(makeDetail("web-1"), "", {
				policies: [
					{
						name: "etc",
						last_job: {
							id: "j",
							state: "failed",
							queued_at: Date.now() - 7_200_000,
							finished_at: Date.now() - 3_600_000,
						},
					},
					policy("db"),
				],
			});
			const card = (
				await screen.findByRole("heading", { name: "Policies" })
			).closest("[data-slot=card]") as HTMLElement;
			expect(
				await within(card).findByRole("link", { name: "etc" }),
			).toHaveAttribute("href", "/policies/etc");
			expect(within(card).getByText("Failed")).toBeInTheDocument();
			expect(within(card).getByText("1 hour ago")).toBeInTheDocument();
			expect(within(card).getByText("Never run")).toBeInTheDocument();
		});

		it("does not ask a revoked host for policies", async () => {
			let asked = false;
			const host = makeDetail("old-1", { status: "revoked" });
			server.use(
				http.get(`/api/v1/hosts/${host.id}/policies`, () => {
					asked = true;
					return HttpResponse.json([]);
				}),
			);
			await openHost(host);
			expect(screen.queryByRole("heading", { name: "Policies" })).toBeNull();
			expect(asked).toBe(false);
		});
	});

	describe("Jobs tab", () => {
		it("shows only this host's jobs without a Host column", async () => {
			const host = makeDetail("web-1");
			const other = makeDetail("web-12");
			jobsHostIdParams.length = 0;
			await openHost(host, "?tab=jobs", {
				jobs: [makeJob(host, { id: "mine" }), makeJob(other, { id: "theirs" })],
			});
			const table = await screen.findByRole("table");
			await waitFor(() =>
				expect(table.querySelectorAll("[data-job]")).toHaveLength(1),
			);
			expect(table.querySelector('[data-job="mine"]')).not.toBeNull();
			expect(
				within(table).queryByRole("columnheader", { name: "Host" }),
			).toBeNull();
			expect(
				screen.getByRole("link", { name: "Open in Jobs" }),
			).toHaveAttribute("href", "/jobs?host=web-1");
			expect(jobsHostIdParams).toContain(host.id);
		});

		it("reruns and cancels jobs from the menu", async () => {
			const host = makeDetail("web-1");
			const reruns: string[] = [];
			const cancels: string[] = [];
			server.use(
				http.post("/api/v1/jobs/:id/rerun", ({ params }) => {
					reruns.push(String(params.id));
					return HttpResponse.json({
						id: "new-1",
						host_id: host.id,
						hostname: host.hostname,
						created: true,
					});
				}),
				http.post("/api/v1/jobs/:id/cancel", ({ params }) => {
					cancels.push(String(params.id));
					return HttpResponse.json(makeJob(host, { id: "run-1" }));
				}),
			);
			const user = userEvent.setup();
			await openHost(host, "?tab=jobs", {
				jobs: [
					makeJob(host, { id: "run-1", state: "running", finished_at: null }),
					makeJob(host, { id: "done-1" }),
				],
			});
			const actions = await screen.findAllByRole("button", {
				name: "Actions for web-1",
			});

			await user.click(actions[0]);
			await user.click(
				await screen.findByRole("menuitem", { name: "Cancel job" }),
			);
			const dialog = await screen.findByRole("alertdialog");
			await user.click(
				within(dialog).getByRole("button", { name: "Cancel job" }),
			);
			await waitFor(() => expect(cancels).toEqual(["run-1"]));

			await user.click(actions[1]);
			await user.click(
				await screen.findByRole("menuitem", { name: "Run again" }),
			);
			await waitFor(() => expect(reruns).toEqual(["done-1"]));
		});

		it("shows an empty state with the illustration", async () => {
			await openHost(makeDetail("web-1"), "?tab=jobs");
			expect(
				await screen.findByText("No jobs on this host yet"),
			).toBeInTheDocument();
			expect(
				document.querySelector('img[src*="preview-host-tabs"]'),
			).not.toBeNull();
		});
	});

	describe("Snapshots tab", () => {
		it("shows policy, stream and incomplete snapshots", async () => {
			await openHost(makeDetail("web-1"), "?tab=snapshots", {
				snapshots: [
					makeSnapshot(),
					makeSnapshot({
						id: "ffeeddcc00112233",
						policy_name: null,
						stdin_name: "app.dump",
						paths: [],
					}),
					makeSnapshot({
						id: "0123456789abcdef",
						paths: ["/var/lib", "/srv"],
						incomplete: true,
					}),
				],
			});
			const table = await screen.findByRole("table");
			const rows = within(table).getAllByRole("row").slice(1);
			expect(rows).toHaveLength(3);
			expect(rows[0]).toHaveTextContent("a1b2c3d4");
			expect(rows[0].querySelectorAll("td")[1].textContent).toBe("a1b2c3d4");
			expect(rows[0]).toHaveTextContent("etc");
			expect(rows[0]).toHaveTextContent("2 hours ago");
			expect(rows[0]).toHaveTextContent("2 KiB");
			expect(rows[0]).toHaveTextContent("3");
			expect(rows[0]).not.toHaveTextContent("Incomplete");
			expect(rows[1]).toHaveTextContent("CLI · app.dump");
			expect(rows[1].querySelectorAll("td")[3]).toHaveTextContent("app.dump");
			expect(rows[2]).toHaveTextContent("/var/lib, /srv");
			expect(rows[2]).toHaveTextContent("Incomplete");
			expect(
				screen.getByText("Open a snapshot in Restore to browse its files."),
			).toBeInTheDocument();
		});

		it("opens a snapshot in Restore from its Browse files link", async () => {
			const detail = makeDetail("web-1");
			server.use(
				http.get("/api/v1/restore/hosts", () => HttpResponse.json([])),
			);
			await openHost(detail, "?tab=snapshots", { snapshots: [makeSnapshot()] });
			const link = await screen.findByRole("link", {
				name: "Browse files in snapshot a1b2c3d4",
			});
			const href = link.getAttribute("href") ?? "";
			expect(href).toContain("/restore?");
			expect(href).toContain(`host=${detail.id}`);
			expect(href).toContain("snapshot=a1b2c3d4e5f60718293a4b5c6d7e8f90");

			await userEvent.click(link);
			expect(
				await screen.findByRole("heading", { level: 1, name: "Restore" }),
			).toBeInTheDocument();
		});

		it("shows an empty state with the illustration", async () => {
			await openHost(makeDetail("web-1"), "?tab=snapshots");
			expect(await screen.findByText("No snapshots yet")).toBeInTheDocument();
			expect(
				document.querySelector('img[src*="preview-host-tabs"]'),
			).not.toBeNull();
		});

		it("pages by 50", async () => {
			await openHost(makeDetail("web-1"), "?tab=snapshots", {
				snapshots: Array.from({ length: 60 }, (_, i) =>
					makeSnapshot({ id: `${String(i).padStart(2, "0")}aaaaaaaaaaaa` }),
				),
			});
			const table = await screen.findByRole("table");
			expect(within(table).getAllByRole("row")).toHaveLength(51);
			expect(screen.getByText("Showing 1-50 of 60")).toBeInTheDocument();

			await userEvent.click(screen.getByRole("button", { name: "Next" }));
			expect(
				await screen.findByText("Showing 51-60 of 60"),
			).toBeInTheDocument();
			await waitFor(() =>
				expect(within(table).getAllByRole("row")).toHaveLength(11),
			);
		});

		it("steps back when the list shrinks below the current page", async () => {
			const host = makeDetail("web-1");
			await openHost(host, "?tab=snapshots", {
				snapshots: Array.from({ length: 60 }, (_, i) =>
					makeSnapshot({ id: `${String(i).padStart(2, "0")}aaaaaaaaaaaa` }),
				),
			});
			await userEvent.click(
				await screen.findByRole("button", { name: "Next" }),
			);
			expect(
				await screen.findByText("Showing 51-60 of 60"),
			).toBeInTheDocument();

			server.use(
				http.get(`/api/v1/hosts/${host.id}/snapshots`, ({ request }) => {
					const page = Number(new URL(request.url).searchParams.get("page"));
					return HttpResponse.json({
						items: page === 1 ? [makeSnapshot({ id: "00aaaaaaaaaaaa" })] : [],
						page,
						page_size: 50,
						total: 1,
					});
				}),
				http.post("/api/v1/repositories/repo-1/catalog/sync", () =>
					HttpResponse.json({ added: 0, skipped: 0, total: 1 }),
				),
			);
			await userEvent.click(
				screen.getByRole("button", { name: "Check storage for new snapshots" }),
			);
			expect(await screen.findByText("Showing 1-1 of 1")).toBeInTheDocument();
		});

		it("syncs the catalog of the host's repository and reloads", async () => {
			const host = makeDetail("web-1");
			let synced = 0;
			let listed = 0;
			await openHost(host, "?tab=snapshots");
			server.use(
				http.get(`/api/v1/hosts/${host.id}/snapshots`, () => {
					listed += 1;
					return HttpResponse.json({
						items: [],
						page: 1,
						page_size: 50,
						total: 0,
					});
				}),
				http.post("/api/v1/repositories/repo-1/catalog/sync", () => {
					synced += 1;
					return HttpResponse.json({ added: 2, skipped: 0, total: 2 });
				}),
			);
			await userEvent.click(
				await screen.findByRole("button", {
					name: "Check storage for new snapshots",
				}),
			);
			expect(
				(await screen.findAllByText("Catalog updated. 2 new snapshots."))
					.length,
			).toBeGreaterThan(0);
			expect(synced).toBe(1);
			await waitFor(() => expect(listed).toBeGreaterThan(0));
		});

		it("reports a storage failure", async () => {
			server.use(
				http.post("/api/v1/repositories/repo-1/catalog/sync", () =>
					HttpResponse.json(
						{ code: "storage", message: "Storage unreachable" },
						{ status: 502 },
					),
				),
			);
			await openHost(makeDetail("web-1"), "?tab=snapshots");
			await userEvent.click(
				await screen.findByRole("button", {
					name: "Check storage for new snapshots",
				}),
			);
			expect(
				(await screen.findAllByText(/Couldn't update the catalog\./)).length,
			).toBeGreaterThan(0);
		});

		it("reports a load failure and retries", async () => {
			const host = makeDetail("web-1");
			await openHost(host);
			server.use(
				http.get(`/api/v1/hosts/${host.id}/snapshots`, () =>
					HttpResponse.json({ code: "x", message: "boom" }, { status: 500 }),
				),
			);
			await userEvent.click(screen.getByRole("tab", { name: "Snapshots" }));
			expect(
				await screen.findByText(/Couldn't load snapshots\./),
			).toBeInTheDocument();

			server.use(
				http.get(`/api/v1/hosts/${host.id}/snapshots`, () =>
					HttpResponse.json({
						items: [makeSnapshot()],
						page: 1,
						page_size: 50,
						total: 1,
					}),
				),
			);
			await userEvent.click(
				screen.getByRole("button", { name: "Retry loading" }),
			);
			expect(await screen.findByText("a1b2c3d4")).toBeInTheDocument();
		});
	});
});
