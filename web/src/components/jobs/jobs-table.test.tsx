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
		routeTree: root.addChildren([stub("/jobs/$id"), stub("/hosts/$id")]),
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
			],
		});
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
