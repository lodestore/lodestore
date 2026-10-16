import {
	act,
	fireEvent,
	screen,
	waitFor,
	within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it, vi } from "vitest";
import type {
	JobDetail,
	JobEventDto,
	JobPage,
	JobSummary,
} from "@/api/gen/model";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

Element.prototype.hasPointerCapture ??= () => false;
Element.prototype.setPointerCapture ??= () => {};
Element.prototype.releasePointerCapture ??= () => {};

const NOW = Date.now();

function makeJob(id: string, over: Partial<JobSummary> = {}): JobSummary {
	return {
		id,
		kind: "backup",
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

function page(items: JobSummary[], total = items.length): JobPage {
	return { items, total, page: 1, page_size: 50 };
}

function mockJobs(respond: (url: URL) => JobPage, seen: URL[] = []): URL[] {
	server.use(
		http.get("/api/v1/jobs", ({ request }) => {
			const url = new URL(request.url);
			seen.push(url);
			return HttpResponse.json(respond(url));
		}),
		http.get("/api/v1/policies", () =>
			HttpResponse.json([
				{
					name: "etc-and-home",
					repository_id: "repo-1",
					repository_name: "main",
					match_labels: [],
					hosts: 1,
					other_repository_hosts: 0,
				},
			]),
		),
	);
	return seen;
}

async function openJobs(path = "/jobs") {
	useSignedIn();
	const app = renderApp(path);
	await screen.findByRole("heading", { name: "Jobs" }, { timeout: 5000 });
	return app;
}

describe("Jobs list", () => {
	it("shows active jobs and history with status and result", async () => {
		mockJobs(() =>
			page([
				makeJob("run-1", {
					state: "running",
					finished_at: null,
					progress: {
						files_done: 50,
						files_total: 100,
						bytes_done: 500,
						bytes_total: 1000,
						bytes_uploaded: 10,
						scan_done: true,
						speed_bps: 100,
						eta_s: 30,
						updated_at: NOW,
					},
				}),
				makeJob("done-1"),
				makeJob("bad-1", {
					state: "failed",
					hostname: "db-1",
					error_message: "repository unreachable",
				}),
			]),
		);
		await openJobs();

		const table = await screen.findByRole("table");
		expect(within(table).getAllByText("web-1", { selector: "a" })).toHaveLength(
			2,
		);
		expect(table.querySelector('[data-status="running"]')).not.toBeNull();
		expect(table.querySelector('[data-status="succeeded"]')).not.toBeNull();
		expect(table.querySelector('[data-status="failed"]')).not.toBeNull();
		expect(within(table).getByText("50%")).toBeInTheDocument();
		expect(within(table).getByText("2 MiB · 1,250 files")).toBeInTheDocument();
		expect(
			within(table).getByText("repository unreachable"),
		).toBeInTheDocument();
	});

	it("puts the status filter in the URL and the request", async () => {
		const seen = mockJobs((url) =>
			url.searchParams.get("status") === "failed"
				? page([makeJob("bad-1", { state: "failed", hostname: "db-1" })])
				: page([makeJob("done-1"), makeJob("bad-1", { state: "failed" })]),
		);
		const { router } = await openJobs();
		await screen.findByRole("table");

		await userEvent.click(
			screen.getByRole("combobox", { name: "Filter by status" }),
		);
		await userEvent.click(
			await screen.findByRole("option", { name: "Failed" }),
		);

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({ status: "failed" }),
		);
		await waitFor(() =>
			expect(seen.at(-1)?.searchParams.get("status")).toBe("failed"),
		);
		expect(await screen.findByText("db-1")).toBeInTheDocument();
	});

	it("reads filters from the link and maps the CLI policy to source=cli", async () => {
		const seen = mockJobs(() =>
			page([makeJob("cli-1", { source: "cli" })], 120),
		);
		await openJobs("/jobs?policy=(cli)&host=web&page=2");
		await waitFor(() => expect(seen.length).toBeGreaterThan(0));

		const last = seen.at(-1);
		expect(last?.searchParams.get("source")).toBe("cli");
		expect(last?.searchParams.get("policy")).toBeNull();
		expect(last?.searchParams.get("host")).toBe("web");
		expect(last?.searchParams.get("page")).toBe("2");
	});

	it("puts the type filter in the URL and the request", async () => {
		const seen = mockJobs((url) =>
			url.searchParams.get("kind") === "restore"
				? page([
						makeJob("r-1", {
							kind: "restore",
							hostname: "db-1",
							host_id: "host-2",
							policy_name: null,
						}),
					])
				: page([makeJob("done-1")]),
		);
		const { router } = await openJobs();
		await screen.findByRole("table");

		await userEvent.click(
			screen.getByRole("combobox", { name: "Filter by type" }),
		);
		expect(
			await screen.findByRole("option", { name: "All types" }),
		).toBeVisible();
		expect(screen.getByRole("option", { name: "Backups" })).toBeVisible();
		expect(screen.getByRole("option", { name: "Downloads" })).toBeVisible();
		await userEvent.click(screen.getByRole("option", { name: "Restores" }));

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({ kind: "restore" }),
		);
		await waitFor(() =>
			expect(seen.at(-1)?.searchParams.get("kind")).toBe("restore"),
		);
		expect(await screen.findByText("db-1")).toBeInTheDocument();
	});

	it("corrects an out-of-range page without adding a history entry", async () => {
		mockJobs(() => page([makeJob("done-1")], 120));
		const { router } = await openJobs("/jobs?page=9");

		await waitFor(() => expect(router.state.location.search.page).toBe(3));
		expect(router.history.canGoBack()).toBe(false);
	});

	it("shows the empty state when nothing ever ran", async () => {
		mockJobs(() => page([]));
		await openJobs();

		expect(await screen.findByText("No jobs yet")).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Create policy" }),
		).toBeInTheDocument();
		expect(screen.queryByRole("table")).toBeNull();
	});

	it("offers to clear filters when none match", async () => {
		mockJobs((url) =>
			url.searchParams.get("status") ? page([]) : page([makeJob("done-1")]),
		);
		const { router } = await openJobs("/jobs?status=failed");

		expect(
			await screen.findByText("No jobs match these filters"),
		).toBeInTheDocument();
		await userEvent.click(
			screen.getByRole("button", { name: "Clear filters" }),
		);

		await waitFor(() => expect(router.state.location.search).toEqual({}));
		expect(await screen.findByRole("table")).toBeInTheDocument();
	});

	it("shows an error with a retry", async () => {
		let fail = true;
		server.use(
			http.get("/api/v1/jobs", () =>
				fail
					? apiError(500, "internal", "boom")
					: HttpResponse.json(page([makeJob("done-1")])),
			),
			http.get("/api/v1/policies", () => HttpResponse.json([])),
		);
		await openJobs();

		expect(await screen.findByText(/Couldn't load jobs/)).toBeInTheDocument();
		fail = false;
		await userEvent.click(
			screen.getByRole("button", { name: "Retry loading" }),
		);
		expect(await screen.findByRole("table")).toBeInTheDocument();
	});

	it("cancels a job through the confirmation dialog", async () => {
		const cancelled: string[] = [];
		mockJobs(() =>
			page([makeJob("run-1", { state: "running", finished_at: null })]),
		);
		server.use(
			http.post("/api/v1/jobs/:id/cancel", ({ params }) => {
				cancelled.push(String(params.id));
				return HttpResponse.json({ ...makeJob("run-1", { state: "running" }) });
			}),
		);
		await openJobs();

		await userEvent.click(
			await screen.findByRole("button", { name: "Actions for web-1" }),
		);
		await userEvent.click(
			await screen.findByRole("menuitem", { name: "Cancel job" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(
			within(dialog).getByRole("button", { name: "Keep running" }),
		).toHaveFocus();
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Cancel job" }),
		);

		await waitFor(() => expect(cancelled).toEqual(["run-1"]));
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
	});

	it("repeats a finished job from the menu", async () => {
		const reruns: string[] = [];
		mockJobs(() => page([makeJob("done-1")]));
		server.use(
			http.post("/api/v1/jobs/:id/rerun", ({ params }) => {
				reruns.push(String(params.id));
				return HttpResponse.json({
					id: "new-1",
					host_id: "host-1",
					hostname: "web-1",
					created: true,
				});
			}),
		);
		await openJobs();

		await userEvent.click(
			await screen.findByRole("button", { name: "Actions for web-1" }),
		);
		await userEvent.click(
			await screen.findByRole("menuitem", { name: "Run again" }),
		);

		await waitFor(() => expect(reruns).toEqual(["done-1"]));
		expect(await screen.findByText("Backup queued for web-1.")).toBeVisible();
		expect(screen.getByRole("button", { name: "View job" })).toBeVisible();
	});
});

function makeDetail(over: Partial<JobDetail> = {}): JobDetail {
	return {
		...makeJob("job-1"),
		repository_id: "repo-1",
		error_detail: {},
		cancel_requested: false,
		log_seq: 0,
		log_lines: 0,
		logs_purged: false,
		policy_exists: true,
		parent_id: null,
		snapshot_id: "0123456789abcdef0123456789abcdef",
		caller_uid: null,
		...over,
	};
}

function progress(over: Partial<NonNullable<JobSummary["progress"]>> = {}) {
	return {
		files_done: 50,
		files_total: 100,
		bytes_done: 500,
		bytes_total: 1000,
		bytes_uploaded: 10,
		scan_done: true,
		speed_bps: 1024,
		eta_s: 240,
		updated_at: Date.now(),
		...over,
	};
}

function mockJob(
	detail: JobDetail | (() => JobDetail),
	events: object = { events: [], last_seq: 0 },
) {
	server.use(
		http.get("/api/v1/jobs/:id", () =>
			HttpResponse.json(typeof detail === "function" ? detail() : detail),
		),
		http.get("/api/v1/jobs/:id/events", () => HttpResponse.json(events)),
	);
}

async function openJob(id = "job-1") {
	useSignedIn();
	const app = renderApp(`/jobs/${id}`);
	await screen.findByRole("heading", { level: 1 }, { timeout: 5000 });
	return app;
}

describe("Job page progress", () => {
	it("keeps the bar undetermined until the scan finishes", async () => {
		mockJob(
			makeDetail({
				state: "running",
				finished_at: null,
				progress: progress({ scan_done: false, eta_s: null }),
			}),
		);
		await openJob();

		const bar = await screen.findByRole("progressbar", {
			name: "Backup progress",
		});
		expect(bar).not.toHaveAttribute("aria-valuenow");
		expect(
			screen.getByText(
				"Scanning the file tree. The time estimate appears when the scan finishes.",
			),
		).toBeInTheDocument();
		const timeLeft = screen.getByText("Time left").closest("div");
		expect(timeLeft).toHaveTextContent("—");
	});

	it("shows percent and time left after the scan", async () => {
		mockJob(
			makeDetail({
				state: "running",
				finished_at: null,
				progress: progress(),
			}),
		);
		await openJob();

		const bar = await screen.findByRole("progressbar", {
			name: "Backup progress",
		});
		expect(bar).toHaveAttribute("aria-valuenow", "50");
		expect(screen.getByText("About 4 min")).toBeInTheDocument();
		expect(screen.getByText("of 100")).toBeInTheDocument();
		expect(screen.getByText("1 KiB/s")).toBeInTheDocument();
		expect(screen.getByText("average of the last 10 s")).toBeInTheDocument();
	});

	it("warns when the host is offline during a run", async () => {
		mockJob(
			makeDetail({
				state: "running",
				finished_at: null,
				host_online: false,
				progress: progress(),
			}),
		);
		await openJob();

		expect(
			await screen.findByText(
				"web-1 is offline. The agent keeps backing up and reports the result when it reconnects.",
			),
		).toBeInTheDocument();
	});

	it("explains why a queued job waits", async () => {
		mockJob(
			makeDetail({
				state: "queued",
				started_at: null,
				finished_at: null,
				host_online: false,
				queue_reason: "host_offline",
				snapshot_id: null,
				stats: null,
			}),
		);
		await openJob();

		expect(
			await screen.findByText(/Waiting for web-1 to connect\./),
		).toHaveTextContent(
			"The job is dropped if the host does not connect within 24 hours.",
		);
		expect(screen.getByRole("button", { name: "Cancel job" })).toBeVisible();
		expect(screen.queryByText("Result")).toBeNull();
	});

	it("offers the warnings chip", async () => {
		mockJob(
			makeDetail({
				state: "running",
				finished_at: null,
				warnings_total: 3,
				progress: progress(),
			}),
		);
		await openJob();

		expect(
			await screen.findByRole("button", { name: "3 warnings" }),
		).toBeVisible();
	});

	it("flags progress that stopped changing, whatever the agent clock says", async () => {
		vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
		try {
			mockJob(
				makeDetail({
					state: "running",
					finished_at: null,
					progress: progress({ updated_at: Date.now() - 3_600_000 }),
				}),
			);
			await openJob();
			expect(screen.queryByText(/^Last update /)).toBeNull();

			act(() => {
				vi.advanceTimersByTime(16_000);
			});

			expect(
				await screen.findByText(/^Last update \d{1,2}:\d\d:\d\d [AP]M$/),
			).toBeVisible();
		} finally {
			vi.useRealTimers();
		}
	});
});

describe("Job page result", () => {
	it("shows the snapshot of a finished job", async () => {
		mockJob(
			makeDetail({
				state: "warnings",
				incomplete: true,
				warnings_total: 2,
			}),
		);
		await openJob();

		const result = (
			await screen.findByRole("heading", { name: "Result" })
		).closest("[data-slot=card]") as HTMLElement;
		expect(within(result).getByText("01234567")).toBeInTheDocument();
		expect(within(result).getByText("None, first backup")).toBeInTheDocument();
		expect(within(result).getByText("Incomplete snapshot")).toBeInTheDocument();
		expect(within(result).getByText("2 MiB")).toBeInTheDocument();
		expect(within(result).getByText("1,250")).toBeInTheDocument();
		expect(
			within(result).getByRole("link", { name: "View host snapshots" }),
		).toHaveAttribute("href", "/hosts/host-1?tab=snapshots");
		expect(screen.getByRole("button", { name: "Run again" })).toBeVisible();
	});

	it("explains a missing backup path", async () => {
		mockJob(
			makeDetail({
				state: "failed",
				error_code: "path_missing",
				error_detail: { path: "/srv/data" },
				snapshot_id: null,
				stats: null,
			}),
		);
		await openJob();

		expect(await screen.findByRole("alert")).toHaveTextContent(
			"A backup path is missing. /srv/data does not exist on web-1. Fix the path in the policy or create it on the host, then run again.",
		);
		expect(screen.queryByRole("heading", { name: "Result" })).toBeNull();
	});

	it("falls back to the server message for other failures", async () => {
		mockJob(
			makeDetail({
				state: "failed",
				error_code: "backup_failed",
				error_message: "repository unreachable.",
				snapshot_id: null,
				stats: null,
			}),
		);
		await openJob();

		expect(await screen.findByRole("alert")).toHaveTextContent(
			"The backup failed. repository unreachable. Check the log, then run again.",
		);
	});

	it("explains an interrupted job", async () => {
		mockJob(
			makeDetail({ state: "interrupted", snapshot_id: null, stats: null }),
		);
		await openJob();

		expect(
			await screen.findByText(
				"The agent stopped while this job ran. Run it again; data already uploaded is not sent twice.",
			),
		).toBeInTheDocument();
	});

	it("hides Run again for command line jobs", async () => {
		mockJob(makeDetail({ source: "cli", policy_name: null, caller_uid: 1000 }));
		await openJob();

		expect(
			await screen.findByText("Run from the command line"),
		).toBeInTheDocument();
		expect(screen.getByText("Command line (uid 1000)")).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Run again" })).toBeNull();
	});

	it("shows a message for an unknown job", async () => {
		server.use(
			http.get("/api/v1/jobs/:id", () =>
				apiError(404, "not_found", "job not found"),
			),
		);
		await openJob("missing");

		expect(await screen.findByText("Job not found")).toBeInTheDocument();
	});
});

describe("Job page live updates", () => {
	it("refetches once on a terminal status and announces it", async () => {
		let requests = 0;
		mockJob(() => {
			requests += 1;
			return requests === 1
				? makeDetail({
						state: "running",
						finished_at: null,
						progress: progress(),
					})
				: makeDetail({ state: "succeeded" });
		});
		await openJob();

		await waitFor(
			() =>
				expect(screen.getByRole("status")).toHaveTextContent("Job succeeded"),
			{ timeout: 4000 },
		);
		await new Promise((resolve) => setTimeout(resolve, 2500));
		expect(requests).toBe(3);
	}, 15_000);
});

function makeEvent(seq: number, over: Partial<JobEventDto> = {}): JobEventDto {
	return {
		seq,
		at: Date.UTC(2026, 9, 15, 10, 0, seq),
		level: "info",
		kind: "info",
		message: `event ${seq}`,
		path: null,
		script: null,
		...over,
	};
}

const runningJob = () =>
	makeDetail({ state: "running", finished_at: null, progress: progress() });

describe("Job log", () => {
	it("loads new events after the last sequence without repeating them", async () => {
		const afters: Array<string | null> = [];
		server.use(
			http.get("/api/v1/jobs/:id", () => HttpResponse.json(runningJob())),
			http.get("/api/v1/jobs/:id/events", ({ request }) => {
				const after = new URL(request.url).searchParams.get("after");
				afters.push(after);
				if (after === null) {
					return HttpResponse.json({
						events: [makeEvent(1), makeEvent(2)],
						last_seq: 2,
					});
				}
				if (after === "2") {
					return HttpResponse.json({
						events: [makeEvent(2), makeEvent(3)],
						last_seq: 3,
					});
				}
				return HttpResponse.json({ events: [], last_seq: Number(after) });
			}),
		);
		await openJob();

		const log = await screen.findByRole("log");
		expect(await within(log).findByText("event 3")).toBeInTheDocument();
		expect(within(log).getAllByText("event 2")).toHaveLength(1);
		expect(afters[0]).toBeNull();
		expect(afters).toContain("2");
		await waitFor(() => expect(afters).toContain("3"));
		expect(log).toHaveAttribute("aria-live", "off");
		expect(log).toHaveAttribute("tabindex", "0");
		expect(within(log).getAllByText("Info")[0]).toHaveClass("sr-only");
	});

	it("filters by level", async () => {
		mockJob(runningJob(), {
			events: [
				makeEvent(1),
				makeEvent(2, { level: "warning", kind: "vanished", path: "/srv/gone" }),
				makeEvent(3, { level: "error", message: "upload failed" }),
			],
			last_seq: 3,
		});
		await openJob();

		const log = await screen.findByRole("log");
		await within(log).findByText("upload failed");
		await userEvent.click(screen.getByRole("radio", { name: "Warnings" }));
		expect(within(log).getByText("Disappeared")).toBeInTheDocument();
		const gone = within(log)
			.getByText("Disappeared")
			.parentElement?.querySelector("code");
		expect(gone).toHaveTextContent("/srv/gone");
		expect(within(log).queryByText("upload failed")).toBeNull();
		expect(within(log).queryByText("event 1")).toBeNull();

		await userEvent.click(screen.getByRole("radio", { name: "Errors" }));
		expect(within(log).getByText("upload failed")).toBeInTheDocument();
		expect(within(log).queryByText("Disappeared")).toBeNull();
	});

	it("folds consecutive hook output into one copyable block", async () => {
		mockJob(makeDetail(), {
			events: [
				makeEvent(1, {
					kind: "hook_output",
					script: "pre.sh",
					message: "dump start",
				}),
				makeEvent(2, {
					kind: "hook_output",
					script: "pre.sh",
					message: "dump done",
				}),
				makeEvent(3, { message: "backup started" }),
			],
			last_seq: 3,
		});
		await openJob();

		const trigger = await screen.findByRole("button", {
			name: "Output of pre.sh (last 64 KiB)",
		});
		expect(screen.getAllByText(/Output of pre\.sh/)).toHaveLength(1);
		expect(screen.queryByText(/dump start/)).toBeNull();
		await userEvent.click(trigger);
		const block = await screen.findByRole("region", {
			name: "Output of pre.sh (last 64 KiB)",
		});
		expect(block).toHaveTextContent("dump start dump done");
		expect(block.textContent).toBe("dump start\ndump done");
		expect(
			screen.getByRole("button", {
				name: "Copy Output of pre.sh (last 64 KiB)",
			}),
		).toBeVisible();
	});

	it("marks the log limit", async () => {
		mockJob(makeDetail(), {
			events: [
				makeEvent(1),
				makeEvent(2, { level: "warning", kind: "log_limit", message: "limit" }),
			],
			last_seq: 2,
		});
		await openJob();

		expect(
			await screen.findByText("Log limit reached. Later events are not kept."),
		).toBeInTheDocument();
		expect(screen.queryByText("limit")).toBeNull();
	});

	it("replaces the feed after the retention period", async () => {
		let asked = 0;
		server.use(
			http.get("/api/v1/jobs/:id", () =>
				HttpResponse.json(makeDetail({ logs_purged: true })),
			),
			http.get("/api/v1/jobs/:id/events", () => {
				asked += 1;
				return HttpResponse.json({ events: [], last_seq: 0 });
			}),
		);
		await openJob();

		expect(
			await screen.findByText(
				"Logs for this job were removed after 30 days. The result above is kept.",
			),
		).toBeInTheDocument();
		expect(screen.queryByRole("log")).toBeNull();
		expect(asked).toBe(0);
	});

	it("says so when a queued job has logged nothing", async () => {
		mockJob(
			makeDetail({
				state: "queued",
				started_at: null,
				finished_at: null,
				snapshot_id: null,
				stats: null,
			}),
		);
		await openJob();

		expect(await screen.findByText("Nothing logged yet.")).toBeInTheDocument();
	});

	it("offers a jump to the latest line after scrolling up", async () => {
		mockJob(runningJob(), {
			events: [makeEvent(1), makeEvent(2)],
			last_seq: 2,
		});
		await openJob();

		const log = await screen.findByRole("log");
		await within(log).findByText("event 2");
		expect(screen.queryByRole("button", { name: "Jump to latest" })).toBeNull();

		let top = 0;
		Object.defineProperty(log, "scrollHeight", {
			configurable: true,
			value: 2000,
		});
		Object.defineProperty(log, "clientHeight", {
			configurable: true,
			value: 480,
		});
		Object.defineProperty(log, "scrollTop", {
			configurable: true,
			get: () => top,
			set: (value: number) => {
				top = value;
			},
		});
		fireEvent.scroll(log);
		const jump = await screen.findByRole("button", { name: "Jump to latest" });
		await userEvent.click(jump);

		expect(top).toBe(2000);
		expect(screen.queryByRole("button", { name: "Jump to latest" })).toBeNull();
	});

	it("turns the warnings chip into the Warnings filter", async () => {
		mockJob(
			makeDetail({
				state: "running",
				finished_at: null,
				warnings_total: 3,
				progress: progress(),
			}),
			{ events: [makeEvent(1)], last_seq: 1 },
		);
		await openJob();

		await userEvent.click(
			await screen.findByRole("button", { name: "3 warnings" }),
		);

		expect(screen.getByRole("radio", { name: "Warnings" })).toHaveAttribute(
			"data-state",
			"on",
		);
		expect(screen.queryByText("event 1")).toBeNull();
	});
});

const SNAP = "0123456789abcdef0123456789abcdef";

function makeRestore(over: Partial<JobDetail> = {}): JobDetail {
	return makeDetail({
		kind: "restore",
		policy_name: null,
		policy_exists: false,
		snapshot_id: null,
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
			items: 3,
			started_by: "admin",
		},
		...over,
	});
}

function makeDownload(over: Partial<JobDetail> = {}): JobDetail {
	return makeDetail({
		kind: "download",
		policy_name: null,
		policy_exists: false,
		snapshot_id: null,
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
}

describe("Restore job page", () => {
	it("shows the phase, storage reads and no empty Result while running", async () => {
		mockJob(
			makeRestore({
				state: "running",
				finished_at: null,
				phase: "restore_files",
				progress: progress({
					scan_done: false,
					bytes_downloaded: 1_048_576,
				}),
			}),
		);
		await openJob();

		expect(
			await screen.findByRole("heading", { name: "Restoring files" }),
		).toBeVisible();
		const bar = screen.getByRole("progressbar", { name: "Restore progress" });
		expect(bar).toHaveAttribute("aria-valuenow", "50");
		expect(screen.getByText("Read from storage")).toBeVisible();
		expect(screen.getByText("data downloaded from the bucket")).toBeVisible();
		expect(screen.getByText("1 MiB")).toBeVisible();
		expect(screen.getByText("Time left")).toBeVisible();
		expect(screen.queryByRole("heading", { name: "Result" })).toBeNull();
		expect(screen.getByRole("button", { name: "Cancel job" })).toBeVisible();
		expect(screen.queryByRole("button", { name: "Run again" })).toBeNull();
		const links = screen.getAllByRole("link", { name: "01234567" });
		expect(links[0]).toHaveAttribute(
			"href",
			`/restore?host=host-2&snapshot=${SNAP}`,
		);
		expect(screen.getByText(/Restore of snapshot/)).toBeVisible();
	});

	it("names the metadata phase", async () => {
		mockJob(
			makeRestore({
				state: "running",
				finished_at: null,
				phase: "restore_metadata",
				progress: progress(),
			}),
		);
		await openJob();
		expect(
			await screen.findByRole("heading", {
				name: "Applying permissions and times",
			}),
		).toBeVisible();
	});

	it("explains an offline host", async () => {
		mockJob(
			makeRestore({
				state: "running",
				finished_at: null,
				host_online: false,
				progress: progress(),
			}),
		);
		await openJob();
		expect(
			await screen.findByText(
				"web-1 is offline. The agent keeps restoring and reports the result when it reconnects.",
			),
		).toBeVisible();
	});

	it("explains a queue wait", async () => {
		mockJob(
			makeRestore({
				state: "queued",
				started_at: null,
				finished_at: null,
				queue_reason: "host_busy",
			}),
		);
		await openJob();
		expect(
			await screen.findByText("Waiting for another job on web-1 to finish."),
		).toBeVisible();
	});

	it("does not blame another job when nothing holds the host", async () => {
		mockJob(
			makeRestore({
				state: "queued",
				started_at: null,
				finished_at: null,
				queue_reason: null,
				host_online: true,
			}),
		);
		await openJob();
		expect(
			(await screen.findAllByText("Waiting in the queue"))[0],
		).toBeVisible();
		expect(screen.queryByText(/Waiting for another job/)).toBeNull();
		expect(screen.queryByText(/to connect/)).toBeNull();
	});

	it("shows the job parameters and the result when it is done", async () => {
		mockJob(
			makeRestore({
				state: "warnings",
				warnings_total: 2,
				restore_result: {
					created: 10,
					overwritten: 4,
					skipped: 1,
					files: 15,
					bytes: 2_097_152,
				},
			}),
		);
		await openJob();

		const jobCard = (
			await screen.findByRole("heading", { name: "Job" })
		).closest("[data-slot=card]") as HTMLElement;
		expect(within(jobCard).getByText("db-1")).toBeVisible();
		const destination = within(jobCard).getByTitle("web-1:/var/tmp/restore");
		expect(destination).toHaveTextContent("web-1:/var/tmp/restore");
		expect(destination.querySelector("wbr")).not.toBeNull();
		expect(within(jobCard).getByText("Existing files")).toBeVisible();
		expect(within(jobCard).getByText("Overwrite changed files")).toBeVisible();
		expect(within(jobCard).getByText("Restore original owners")).toBeVisible();
		expect(within(jobCard).getByText("admin")).toBeVisible();

		const result = screen
			.getByRole("heading", { name: "Result" })
			.closest("[data-slot=card]") as HTMLElement;
		for (const [label, value] of [
			["Created", "10"],
			["Overwritten", "4"],
			["Skipped", "1"],
			["Warnings", "2"],
		]) {
			expect(
				within(result).getByText(label).nextElementSibling,
			).toHaveTextContent(value);
		}
		expect(screen.queryByRole("button", { name: "Run again" })).toBeNull();
	});

	it("cancels with the restore wording", async () => {
		mockJob(
			makeRestore({
				state: "running",
				finished_at: null,
				progress: progress(),
			}),
		);
		await openJob();
		await userEvent.click(
			await screen.findByRole("button", { name: "Cancel job" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(within(dialog).getByText("Cancel this restore?")).toBeVisible();
		expect(
			within(dialog).getByText(
				"The host stops writing. Files already written stay on the host, so run it again to finish.",
			),
		).toBeVisible();
	});

	it("explains a destination that is not empty with the path", async () => {
		mockJob(
			makeRestore({
				state: "failed",
				error_code: "restore_target_not_empty",
				error_message: "target not empty",
			}),
		);
		await openJob();
		const alert = await screen.findByRole("alert");
		expect(alert).toHaveTextContent(
			"The destination folder is not empty. /var/tmp/restore on web-1 has content and the restore was set to stop. Choose an empty folder or another option, then restore again.",
		);
		expect(alert.querySelector("wbr")).not.toBeNull();
	});

	it("falls back to the server message for other restore errors", async () => {
		mockJob(
			makeRestore({
				state: "failed",
				error_code: "restore_agent_outdated",
				error_message: "Update the agent on this host to restore to it.",
			}),
		);
		await openJob();
		expect(
			await screen.findByText(
				"The restore failed. Update the agent on this host to restore to it. Check the log, then restore again.",
			),
		).toBeVisible();
	});

	it("labels restore warnings in the log", async () => {
		mockJob(makeRestore({ state: "warnings", warnings_total: 1 }), {
			events: [
				makeEvent(1, {
					level: "warning",
					kind: "owner_not_applied",
					path: "/etc/app.conf",
					message: "chown failed",
				}),
			],
			last_seq: 1,
		});
		await openJob();
		const log = await screen.findByRole("log");
		expect(await within(log).findByText("Owner not applied")).toBeVisible();
	});
});

describe("Download job page", () => {
	it("shows the stream phase with Sent and Speed and no bar or actions", async () => {
		mockJob(
			makeDownload({
				state: "running",
				finished_at: null,
				progress: progress({ bytes_done: 2_097_152, speed_bps: 1024 }),
			}),
		);
		await openJob();

		expect(
			await screen.findByRole("heading", { name: "Streaming the archive" }),
		).toBeVisible();
		expect(screen.getByText("Sent")).toBeVisible();
		expect(screen.getByText("Speed")).toBeVisible();
		expect(screen.queryByRole("progressbar")).toBeNull();
		expect(screen.queryByRole("button", { name: "Cancel job" })).toBeNull();
		expect(screen.queryByRole("button", { name: "Run again" })).toBeNull();
		expect(screen.queryByRole("log")).toBeNull();
	});

	it("shows the job card and what was sent", async () => {
		mockJob(makeDownload());
		await openJob();

		const jobCard = (
			await screen.findByRole("heading", { name: "Job" })
		).closest("[data-slot=card]") as HTMLElement;
		expect(within(jobCard).getByText("tar.gz")).toBeVisible();
		expect(within(jobCard).getByText("2 items")).toBeVisible();
		const result = screen
			.getByRole("heading", { name: "Result" })
			.closest("[data-slot=card]") as HTMLElement;
		expect(within(result).getByText("3 MiB")).toBeVisible();
	});

	it("says the archive is incomplete when the connection closed", async () => {
		mockJob(makeDownload({ state: "interrupted" }));
		await openJob();
		expect(
			await screen.findByText(
				"The connection closed before the archive was complete. The file you received is incomplete. Download it again.",
			),
		).toBeVisible();
	});

	it("shows a host without an agent as plain text", async () => {
		mockJob(
			makeDownload({ host_id: "agentless:old-box", hostname: "old-box" }),
		);
		await openJob();
		const jobCard = (
			await screen.findByRole("heading", { name: "Job" })
		).closest("[data-slot=card]") as HTMLElement;
		expect(within(jobCard).getByText("old-box")).toBeVisible();
		expect(within(jobCard).queryByRole("link", { name: "old-box" })).toBeNull();
	});
});

describe("Backup job page link to Restore", () => {
	it("links the snapshot to Restore", async () => {
		mockJob(makeDetail());
		await openJob();
		expect(
			await screen.findByRole("link", { name: "Browse files" }),
		).toHaveAttribute("href", `/restore?host=host-1&snapshot=${SNAP}`);
		expect(
			screen.getByRole("link", { name: "View host snapshots" }),
		).toBeVisible();
	});
});
