import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { type JobRow, type JobState, JobsTable, toJobRow } from "./jobs-table";

const NOW = Date.now();

function job(over: Partial<JobRow> = {}): JobRow {
	return {
		id: "job-1",
		host_id: "host-1",
		hostname: "web-1",
		policy_name: "etc-and-home",
		policy_exists: true,
		source: "manual",
		state: "succeeded",
		queued_at: NOW - 120_000,
		started_at: NOW - 100_000,
		finished_at: NOW - 40_000,
		host_online: true,
		incomplete: false,
		warnings_total: 0,
		errors_total: 0,
		stats: {
			bytes_total: 2_097_152,
			files_new: 10,
			files_changed: 5,
			files_unchanged: 1_235,
		},
		...over,
	};
}

function renderTable(props: Parameters<typeof JobsTable>[0]) {
	const root = createRootRoute({
		component: () => (
			<TooltipProvider>
				<JobsTable {...props} />
			</TooltipProvider>
		),
	});
	const stub = (path: string) =>
		createRoute({ getParentRoute: () => root, path, component: () => null });
	const router = createRouter({
		routeTree: root.addChildren([
			stub("/jobs/$id"),
			stub("/hosts/$id"),
			stub("/settings/repositories/$id"),
		]),
		history: createMemoryHistory({ initialEntries: ["/"] }),
	});
	render(<RouterProvider router={router as never} />);
	return router;
}

const rowOf = async (id = "job-1") =>
	(await screen.findByRole("table")).querySelector(
		`[data-job="${id}"]`,
	) as HTMLElement;

describe("JobsTable", () => {
	it.each<JobState>([
		"queued",
		"running",
		"waiting",
		"succeeded",
		"warnings",
		"failed",
		"cancelled",
		"interrupted",
		"not_started",
	])("marks a %s row with its status", async (state) => {
		renderTable({ jobs: [job({ state })] });
		const row = await rowOf();
		expect(row.querySelector(`[data-status="${state}"]`)).not.toBeNull();
	});

	it("shows a determinate bar and percent once the scan is done", async () => {
		renderTable({
			jobs: [
				job({
					state: "running",
					finished_at: null,
					progress: {
						files_done: 5,
						files_total: 10,
						bytes_done: 50,
						bytes_total: 200,
						bytes_uploaded: 10,
						scan_done: true,
						speed_bps: 1,
						updated_at: NOW,
					},
				}),
			],
		});
		const bar = within(await rowOf()).getByRole("progressbar");
		expect(bar).toHaveAttribute("aria-valuenow", "25");
		expect(within(await rowOf()).getByText("25%")).toBeInTheDocument();
	});

	it("shows the file count while the scan is running", async () => {
		renderTable({
			jobs: [
				job({
					state: "running",
					finished_at: null,
					progress: {
						files_done: 0,
						files_total: 1_234,
						bytes_done: 0,
						bytes_total: 0,
						bytes_uploaded: 0,
						scan_done: false,
						speed_bps: 0,
						updated_at: NOW,
					},
				}),
			],
		});
		const row = await rowOf();
		expect(within(row).getByText("Scanning · 1,234 files")).toBeInTheDocument();
		expect(within(row).queryByRole("progressbar")).toBeNull();
	});

	it("explains why a queued job waits", async () => {
		renderTable({
			jobs: [
				job({
					id: "a",
					state: "queued",
					queue_reason: "host_offline",
					host_online: false,
				}),
				job({ id: "b", state: "queued", queue_reason: "host_busy" }),
				job({ id: "c", state: "queued", queue_reason: null }),
			],
		});
		expect(
			within(await rowOf("c")).getByText("Waiting in the queue"),
		).toBeInTheDocument();
		expect(
			within(await rowOf("a")).getByText("Waiting for the host to connect"),
		).toBeInTheDocument();
		expect(
			within(await rowOf("b")).getByText(
				"Waiting for another backup on this host",
			),
		).toBeInTheDocument();
		expect(within(await rowOf("a")).getByText(/^Queued /)).toBeInTheDocument();
	});

	it("summarises finished jobs", async () => {
		renderTable({
			jobs: [
				job({ id: "ok" }),
				job({
					id: "warn",
					state: "warnings",
					warnings_total: 3,
					incomplete: true,
				}),
				job({
					id: "bad",
					state: "failed",
					error_message: "path /srv is missing\nsecond line",
				}),
			],
		});
		expect(
			within(await rowOf("ok")).getByText("2 MiB · 1,250 files"),
		).toBeInTheDocument();
		expect(
			within(await rowOf("warn")).getByText("2 MiB · 3 warnings"),
		).toBeInTheDocument();
		const bad = within(await rowOf("bad"));
		expect(bad.getByText("path /srv is missing")).toBeInTheDocument();
		expect(bad.queryByText(/second line/)).toBeNull();
	});

	it("jobs table origin suffix and suspicious icon", async () => {
		renderTable({
			jobs: [
				job({ id: "sch", origin: "schedule" }),
				job({ id: "cu", origin: "catchup" }),
				job({ id: "man", origin: "manual" }),
				job({ id: "sus", snapshot_suspicious: true }),
			],
		});
		expect(
			within(await rowOf("sch")).getByText("Policy etc-and-home · Scheduled"),
		).toBeInTheDocument();
		expect(
			within(await rowOf("cu")).getByText("Policy etc-and-home · Catch-up"),
		).toBeInTheDocument();
		const man = within(await rowOf("man"));
		expect(man.getByText("Policy etc-and-home")).toBeInTheDocument();
		expect(man.queryByText("Suspicious snapshot")).toBeNull();
		const sus = within(await rowOf("sus"));
		expect(sus.getByText("Suspicious snapshot")).toHaveClass("sr-only");
		expect(sus.getByText("2 MiB · 1,250 files")).toBeInTheDocument();
	});

	it("shows a CLI badge instead of a policy line for command-line jobs", async () => {
		renderTable({
			jobs: [job({ id: "cli", source: "cli", policy_name: null })],
		});
		const row = within(await rowOf("cli"));
		expect(row.getByText("CLI")).toBeInTheDocument();
		expect(row.queryByText(/^Policy /)).toBeNull();
	});

	it("hides the host column and the policy line on request", async () => {
		renderTable({ jobs: [job()], hide: ["host"] });
		const table = await screen.findByRole("table");
		expect(
			within(table).queryByRole("columnheader", { name: "Host" }),
		).toBeNull();
		expect(within(table).queryByRole("link", { name: "web-1" })).toBeNull();
		expect(within(table).getByText("Policy etc-and-home")).toBeInTheDocument();
	});

	it("drops the policy line with hide policy", async () => {
		renderTable({ jobs: [job()], hide: ["policy"] });
		const row = within(await rowOf());
		expect(row.getByRole("link", { name: "web-1" })).toBeInTheDocument();
		expect(row.queryByText(/^Policy /)).toBeNull();
	});

	it("offers Cancel only for active jobs and Run again only for finished manual jobs", async () => {
		const user = userEvent.setup();
		const onCancel = vi.fn();
		const onRerun = vi.fn();
		const jobs = [
			job({ id: "q", state: "queued" }),
			job({ id: "done" }),
			job({
				id: "cli",
				source: "cli",
				policy_name: null,
				policy_exists: false,
			}),
			job({ id: "gone", policy_exists: false }),
		];
		renderTable({ jobs, onCancel, onRerun });
		await user.click(
			within(await rowOf("q")).getByRole("button", {
				name: "Actions for web-1",
			}),
		);
		expect(screen.queryByRole("menuitem", { name: "Run again" })).toBeNull();
		await user.click(screen.getByRole("menuitem", { name: "Cancel job" }));
		expect(onCancel).toHaveBeenCalledWith(jobs[0]);

		await user.click(
			within(await rowOf("done")).getByRole("button", {
				name: "Actions for web-1",
			}),
		);
		expect(screen.queryByRole("menuitem", { name: "Cancel job" })).toBeNull();
		await user.click(screen.getByRole("menuitem", { name: "Run again" }));
		expect(onRerun).toHaveBeenCalledWith(jobs[1]);

		await user.click(
			within(await rowOf("cli")).getByRole("button", {
				name: "Actions for web-1",
			}),
		);
		expect(screen.queryByRole("menuitem", { name: "Run again" })).toBeNull();
		expect(screen.queryByRole("menuitem", { name: "Cancel job" })).toBeNull();
		await user.keyboard("{Escape}");

		await user.click(
			within(await rowOf("gone")).getByRole("button", {
				name: "Actions for web-1",
			}),
		);
		expect(screen.getByRole("menuitem", { name: "View job" })).toBeVisible();
		expect(screen.queryByRole("menuitem", { name: "Run again" })).toBeNull();
	});

	it("renders five skeleton rows while loading", async () => {
		renderTable({ jobs: [], loading: true });
		const table = await screen.findByRole("table");
		expect(table.querySelectorAll("tbody tr")).toHaveLength(5);
		expect(
			table.querySelectorAll('[data-slot="skeleton"]').length,
		).toBeGreaterThan(0);
	});

	it("opens the job from a row click and keeps the host link on the host", async () => {
		const user = userEvent.setup();
		const router = renderTable({ jobs: [job()] });
		const row = await rowOf();
		await user.click(within(row).getByRole("link", { name: "web-1" }));
		expect(router.state.location.pathname).toBe("/hosts/host-1");
		await router.navigate({ to: "/" as never });
		await user.click(within(row).getByText("2 MiB · 1,250 files"));
		expect(router.state.location.pathname).toBe("/jobs/job-1");
	});
});

describe("toJobRow", () => {
	it("fills the progress and stats fields the server omits", () => {
		const row = toJobRow({
			...job(),
			progress: { files_done: 3 },
			stats: { files_new: 1 },
		} as never);
		expect(row.progress).toMatchObject({
			files_done: 3,
			bytes_done: 0,
			speed_bps: 0,
		});
		expect(row.stats).toMatchObject({ files_new: 1, bytes_total: 0 });
	});

	it("keeps absent progress and stats absent", () => {
		const row = toJobRow({
			...job(),
			progress: null,
			stats: undefined,
		} as never);
		expect(row.progress).toBeNull();
		expect(row.stats).toBeUndefined();
	});
});

describe("JobsTable restore and download rows", () => {
	const SNAP = "0123456789abcdef0123456789abcdef";
	const restore = (over: Partial<JobRow> = {}) =>
		job({
			kind: "restore",
			policy_name: null,
			policy_exists: true,
			stats: null,
			restore: {
				snapshot_id: SNAP,
				source_host_key: "host-2",
				source_hostname: "db-1",
				destination: "/var/tmp/restore",
				in_place: false,
				conflict: "overwrite_changed",
				restore_owners: true,
				other_repository: false,
				items: 2,
				started_by: "admin",
			},
			...over,
		});
	const download = (over: Partial<JobRow> = {}) =>
		job({
			id: "dl-1",
			kind: "download",
			policy_name: null,
			policy_exists: false,
			stats: null,
			download: {
				snapshot_id: SNAP,
				format: "tar_gz",
				items: 2,
				bytes_sent: 3_145_728,
				started_by: "admin",
			},
			...over,
		});

	it("names the snapshot under the host and shows a determinate bar while running", async () => {
		renderTable({
			jobs: [
				restore({
					state: "running",
					finished_at: null,
					progress: {
						files_done: 5,
						files_total: 10,
						bytes_done: 50,
						bytes_total: 200,
						bytes_uploaded: 0,
						scan_done: false,
						speed_bps: 1,
						updated_at: NOW,
					},
				}),
			],
		});
		const row = await rowOf();
		expect(within(row).getByText("Restore · snapshot 01234567")).toBeVisible();
		const bar = within(row).getByRole("progressbar", {
			name: "Restore progress",
		});
		expect(bar).toHaveAttribute("aria-valuenow", "25");
		expect(within(row).getByText("25%")).toBeInTheDocument();
	});

	it("summarises finished restores", async () => {
		renderTable({
			jobs: [
				restore({
					id: "ok",
					restore_result: { bytes: 2_097_152, files: 1_250 },
				}),
				restore({
					id: "warn",
					state: "warnings",
					warnings_total: 3,
					restore_result: { bytes: 2_097_152, files: 1_250 },
				}),
			],
		});
		expect(
			within(await rowOf("ok")).getByText("2 MiB · 1,250 files"),
		).toBeInTheDocument();
		expect(
			within(await rowOf("warn")).getByText("2 MiB · 3 warnings"),
		).toBeInTheDocument();
	});

	it("explains a failed restore the way the job page does", async () => {
		renderTable({
			jobs: [
				restore({
					id: "denied",
					state: "failed",
					error_code: "restore_target_unwritable",
					error_message: "Permission denied (os error 13)",
				}),
				restore({
					id: "other",
					state: "failed",
					error_code: null,
					error_message: "Permission denied (os error 13)",
				}),
			],
		});
		expect(
			within(await rowOf("denied")).getByText(
				/^The destination is not writable\./,
			),
		).toBeInTheDocument();
		expect(
			within(await rowOf("other")).getByText(
				/^The restore failed\. Permission denied \(os error 13\)\./,
			),
		).toBeInTheDocument();
	});

	it("offers Cancel for restores and never Run again", async () => {
		const user = userEvent.setup();
		renderTable({
			jobs: [
				restore({ id: "q", state: "queued" }),
				restore({ id: "done" }),
				download({ id: "dl-run", state: "running" }),
			],
			onCancel: vi.fn(),
			onRerun: vi.fn(),
		});
		const open = async (id: string) =>
			user.click(
				within(await rowOf(id)).getByRole("button", {
					name: "Actions for web-1",
				}),
			);

		await open("q");
		expect(screen.getByRole("menuitem", { name: "Cancel job" })).toBeVisible();
		expect(screen.queryByRole("menuitem", { name: "Run again" })).toBeNull();
		await user.keyboard("{Escape}");

		await open("done");
		expect(screen.queryByRole("menuitem", { name: "Run again" })).toBeNull();
		await user.keyboard("{Escape}");

		await open("dl-run");
		expect(screen.getByRole("menuitem", { name: "View job" })).toBeVisible();
		expect(screen.queryByRole("menuitem", { name: "Cancel job" })).toBeNull();
	});

	it("shows what a download sent and flags an incomplete archive", async () => {
		renderTable({
			jobs: [
				download(),
				download({ id: "dl-2", state: "interrupted" }),
				download({ id: "dl-3", state: "running" }),
				download({ id: "dl-4", state: "failed", error_message: null }),
			],
		});
		expect(
			within(await rowOf("dl-4")).getByText(
				"The archive could not be built. Download it again.",
			),
		).toBeVisible();
		const row = await rowOf("dl-1");
		expect(within(row).getByText("Download · tar.gz")).toBeVisible();
		expect(within(row).getByText("3 MiB sent · tar.gz")).toBeVisible();
		expect(
			within(await rowOf("dl-2")).getByText("Archive incomplete"),
		).toBeVisible();
		expect(
			within(await rowOf("dl-3")).getByText("Streaming the archive"),
		).toBeVisible();
	});

	it("shows the name of a host without an agent as plain text", async () => {
		renderTable({
			jobs: [download({ host_id: "agentless:old-box", hostname: "old-box" })],
		});
		const row = await rowOf("dl-1");
		expect(within(row).getByText("old-box")).toBeVisible();
		expect(within(row).queryByRole("link", { name: "old-box" })).toBeNull();
	});
});

function gcJob(
	over: Partial<JobRow> = {},
	gc: Partial<NonNullable<JobRow["gc"]>> = {},
): JobRow {
	return job({
		id: "gc-1",
		kind: "gc",
		host_id: "",
		hostname: "Main repo",
		policy_name: null,
		stats: null,
		gc: {
			repository_id: "repo-1",
			repository_name: "Main repo",
			dry_run: false,
			origin: "manual",
			phase: "done",
			postponed: false,
			stopped_while_deleting: false,
			...gc,
		},
		...over,
	});
}

describe("JobsTable cleanup rows", () => {
	it("gc row links to the repository card", async () => {
		const user = userEvent.setup();
		const router = renderTable({ jobs: [gcJob()] });
		const row = await rowOf("gc-1");
		const link = within(row).getByRole("link", {
			name: "Repository Main repo",
		});
		expect(link).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#free-up-space",
		);
		expect(within(row).getByText("Cleanup")).toBeVisible();
		await user.click(within(row).getByText("Nothing to free"));
		expect(router.state.location.pathname).toBe(
			"/settings/repositories/repo-1",
		);
		expect(router.state.location.hash).toBe("free-up-space");
	});

	it("gc row shows phase and progress while running", async () => {
		renderTable({
			jobs: [
				gcJob(
					{ state: "running", finished_at: null },
					{ phase: "repacking", progress: { done: 1, total: 4, bytes: 10 } },
				),
			],
		});
		const row = await rowOf("gc-1");
		expect(within(row).getByText("Rewriting packs")).toBeVisible();
		expect(within(row).getByRole("progressbar")).toHaveAttribute(
			"aria-valuenow",
			"25",
		);
	});

	it("gc dry run row shows would free", async () => {
		renderTable({
			jobs: [
				gcJob({}, { dry_run: true, bytes_freed: 3_221_225_472 }),
				gcJob({ id: "gc-2" }, { bytes_freed: 1_048_576, origin: "schedule" }),
				gcJob({ id: "gc-3" }, { bytes_freed: 0 }),
			],
		});
		const dry = await rowOf("gc-1");
		expect(within(dry).getByText("Would free 3 GiB")).toBeVisible();
		expect(within(dry).getByText("Cleanup · dry run")).toBeVisible();
		const real = await rowOf("gc-2");
		expect(within(real).getByText("Freed 1 MiB")).toBeVisible();
		expect(within(real).getByText("Cleanup · scheduled")).toBeVisible();
		expect(
			within(await rowOf("gc-3")).getByText("Nothing to free"),
		).toBeVisible();
	});

	it("gc row has no actions menu", async () => {
		renderTable({ jobs: [gcJob()] });
		const row = await rowOf("gc-1");
		expect(within(row).queryByRole("button")).toBeNull();
	});

	it("gc waiting row shows delete date", async () => {
		const at = new Date(2026, 9, 24, 3, 0).getTime();
		renderTable({
			jobs: [
				gcJob({ state: "waiting" }, { phase: "waiting", delete_after: at }),
			],
		});
		const row = await rowOf("gc-1");
		expect(within(row).getByText(/^Deletes after .*24, 2026/)).toBeVisible();
		expect(row.querySelector('[data-status="waiting"]')).not.toBeNull();
		expect(within(row).queryByText(/Running|Postponed/)).toBeNull();
	});

	it("gc postponed row", async () => {
		renderTable({
			jobs: [
				gcJob(
					{ state: "waiting" },
					{ phase: "waiting", postponed: true, delete_after: Date.now() },
				),
			],
		});
		expect(
			within(await rowOf("gc-1")).getByText("Postponed by unfinished backups"),
		).toBeVisible();
	});

	it("gc row shows object lock hint", async () => {
		const user = userEvent.setup();
		const until = new Date(2026, 9, 24, 12, 0).getTime();
		renderTable({
			jobs: [
				gcJob(
					{},
					{
						bytes_freed: 1_048_576,
						locked_bytes: 3_435_973_837,
						locked_until: until,
					},
				),
				gcJob({ id: "gc-2" }, { bytes_freed: 1_048_576, locked_bytes: 0 }),
			],
		});
		const row = await rowOf("gc-1");
		const mark = within(row).getByText("Some data is held by Object Lock");
		await user.hover(mark);
		expect(
			(await screen.findAllByText("3.2 GiB frees after 24 Oct 2026"))[0],
		).toBeInTheDocument();
		expect(
			within(await rowOf("gc-2")).queryByText(
				"Some data is held by Object Lock",
			),
		).toBeNull();
	});

	it("gc failed row says whether anything was deleted", async () => {
		renderTable({
			jobs: [
				gcJob({ state: "failed" }, { phase: "failed" }),
				gcJob(
					{ id: "gc-2", state: "failed" },
					{ phase: "failed", stopped_while_deleting: true },
				),
			],
		});
		expect(
			within(await rowOf("gc-1")).getByText("Stopped, nothing deleted"),
		).toBeVisible();
		expect(
			within(await rowOf("gc-2")).getByText("Stopped while deleting"),
		).toBeVisible();
	});
});

function checkJob(
	over: Partial<JobRow> = {},
	check: Partial<NonNullable<JobRow["check"]>> = {},
): JobRow {
	return job({
		id: "check-1",
		kind: "check",
		host_id: "",
		hostname: "main",
		policy_name: null,
		stats: null,
		check: {
			repository_id: "repo-1",
			repository_name: "main",
			level: "structure",
			origin: "manual",
			findings_total: 0,
			...check,
		},
		...over,
	});
}

describe("JobsTable check rows", () => {
	it("renders check rows", async () => {
		const router = renderTable({
			jobs: [
				checkJob(
					{ state: "running" },
					{
						level: "packs",
						origin: "schedule",
						phase: "packs",
						progress: { phase: "packs", done: 3, total: 10, bytes: 1024 },
					},
				),
			],
		});
		const row = await rowOf("check-1");
		const link = within(row).getByRole("link", { name: "Repository main" });
		expect(link).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#checks",
		);
		expect(
			within(row).getByText("Check · pack reading · scheduled"),
		).toBeVisible();
		expect(within(row).getByRole("progressbar")).toBeVisible();
		expect(within(row).getByText("Reading packs")).toBeVisible();
		expect(within(row).queryByRole("button")).toBeNull();

		await userEvent.click(within(row).getByText("Reading packs"));
		expect(router.state.location.pathname).toBe(
			"/settings/repositories/repo-1",
		);
		expect(router.state.location.hash).toBe("checks");
	});

	it("renders check results", async () => {
		const reason = "Stopped at the size limit.";
		renderTable({
			jobs: [
				checkJob({ id: "c-ok", state: "succeeded" }),
				checkJob(
					{ id: "c-bad", state: "failed" },
					{ outcome: "problems", findings_total: 2 },
				),
				checkJob(
					{ id: "c-stop", state: "failed" },
					{ outcome: "stopped", error: reason },
				),
				checkJob(
					{ id: "c-long", state: "failed" },
					{ outcome: "stopped", error: "x".repeat(200) },
				),
				checkJob({ id: "c-cancel", state: "cancelled" }),
			],
		});
		expect(within(await rowOf("c-ok")).getByText("Passed")).toBeVisible();
		expect(within(await rowOf("c-bad")).getByText("2 problems")).toBeVisible();
		expect(
			within(await rowOf("c-stop")).getByText(`Stopped: ${reason}`),
		).toBeVisible();
		expect(
			within(await rowOf("c-long")).getByText(`Stopped: ${"x".repeat(200)}`),
		).toBeVisible();
		const cells = (await rowOf("c-cancel")).querySelectorAll("td");
		expect(cells[cells.length - 2]).toHaveTextContent("Cancelled");
	});
});
