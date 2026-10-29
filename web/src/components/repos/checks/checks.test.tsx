import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { CheckRunDto } from "@/api/gen/model";
import { apiError } from "@/test/app";
import {
	checkRun,
	checkStatus,
	checksCard,
	levelStatus,
	openChecks,
	packsRun,
	restoreRun,
	scheduleInfo,
} from "@/test/checks";
import { server } from "@/test/server";

const GIB = 1024 ** 3;

const runningPacks = (over: Partial<CheckRunDto> = {}) =>
	packsRun({
		id: "run-3",
		state: "running",
		outcome: null,
		finished_at: null,
		duration_ms: null,
		progress: { phase: "packs", done: 3, total: 10, bytes: GIB },
		...over,
	});

const row = (view: Awaited<ReturnType<typeof checksCard>>, name: string) =>
	within(
		view
			.getByText(name, { selector: "[data-slot=item-title]" })
			.closest("[data-slot=item]") as HTMLElement,
	);

const scheduled = () =>
	checkStatus({
		levels: [
			levelStatus("structure", { schedule: scheduleInfo("daily") }),
			levelStatus("packs", { schedule: scheduleInfo("weekly") }),
			levelStatus("restore", { schedule: scheduleInfo("monthly") }),
		],
	});

async function openSettings(view: Awaited<ReturnType<typeof checksCard>>) {
	await userEvent.click(await view.findByRole("button", { name: /^Settings/ }));
}

const group = (view: Awaited<ReturnType<typeof checksCard>>, name: string) =>
	within(view.getByRole("group", { name }));

describe("integrity checks card", () => {
	it("shows levels with their last result", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure", { last: checkRun() }),
					levelStatus("packs"),
					levelStatus("restore"),
				],
			}),
		);
		const view = await checksCard();
		expect(await view.findByText("Structure")).toBeInTheDocument();
		expect(view.getByText("Pack reading")).toBeInTheDocument();
		expect(
			view.getByText(
				"Listing, indexes, snapshot and tree references, pack presence.",
			),
		).toBeInTheDocument();
		expect(view.getByText("Passed")).toBeInTheDocument();
		expect(view.getAllByText("Never run")).toHaveLength(2);
		expect(view.getAllByRole("button", { name: "Run now" })).toHaveLength(3);
		expect(row(view, "Pack reading").queryByText("Show report")).toBeNull();
		expect(view.queryByRole("alert")).not.toBeInTheDocument();
	});

	it("runs a level and shows progress", async () => {
		const current = openChecks();
		let started = "";
		server.use(
			http.post(
				"/api/v1/repositories/repo-1/checks/:level/run",
				({ params }) => {
					started = String(params.level);
					current.status = checkStatus({
						levels: [
							levelStatus("structure"),
							levelStatus("packs", { running: runningPacks() }),
						],
					});
					return HttpResponse.json({ run_id: "run-3" }, { status: 202 });
				},
			),
		);
		const view = await checksCard();
		await view.findByText("Pack reading");
		const run = row(view, "Pack reading").getByRole("button", {
			name: "Run now",
		});
		await userEvent.click(run);
		expect(await view.findByText("Reading packs")).toBeInTheDocument();
		expect(started).toBe("packs");
		expect(view.getByText("3 of 10 packs · 1 GiB")).toBeInTheDocument();
		expect(view.getByRole("progressbar")).toBeInTheDocument();
		expect(view.getByRole("status")).toHaveTextContent("Reading packs");
		expect(
			view.getByText("Another check is running on this repository."),
		).toBeInTheDocument();
		expect(
			row(view, "Structure").getByRole("button", { name: "Run now" }),
		).toHaveAttribute("aria-disabled", "true");
	});

	it("cancels a running check", async () => {
		const current = openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs", { running: runningPacks() }),
				],
			}),
		);
		let cancelled = "";
		server.use(
			http.post(
				"/api/v1/repositories/repo-1/checks/runs/:run/cancel",
				({ params }) => {
					cancelled = String(params.run);
					current.status = checkStatus();
					return HttpResponse.json({ run_id: "run-3" });
				},
			),
		);
		const view = await checksCard();
		await userEvent.click(
			await view.findByRole("button", { name: "Cancel check run" }),
		);
		await waitFor(() =>
			expect(view.getAllByRole("button", { name: "Run now" })).toHaveLength(3),
		);
		expect(cancelled).toBe("run-3");
		expect(view.queryByText("Reading packs")).not.toBeInTheDocument();
	});

	it("blocks run during cleanup", async () => {
		openChecks(
			checkStatus({
				blocked: { reason: "cleanup", message: "cleanup is running" },
			}),
		);
		let posted = false;
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/:level/run", () => {
				posted = true;
				return HttpResponse.json({ run_id: "x" }, { status: 202 });
			}),
		);
		const view = await checksCard();
		const [first] = await view.findAllByRole("button", { name: "Run now" });
		expect(first).toHaveAttribute("aria-disabled", "true");
		expect(
			view.getByText("A cleanup cycle is running. Checks wait for it."),
		).toBeInTheDocument();
		await userEvent.click(first);
		expect(posted).toBe(false);
	});

	it("shows the server message when the start is refused", async () => {
		const current = openChecks();
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/:level/run", () => {
				current.status = checkStatus({
					blocked: { reason: "check", message: "busy" },
				});
				return apiError(409, "conflict", "a check is already running");
			}),
		);
		const view = await checksCard();
		await userEvent.click(
			(await view.findAllByRole("button", { name: "Run now" }))[0],
		);
		expect(
			await view.findByText(
				"Couldn't start the check. A check is already running. Try again.",
			),
		).toBeInTheDocument();
		expect(
			await view.findByText("Another check is running on this repository."),
		).toBeInTheDocument();
	});

	it("shows the load error", async () => {
		openChecks();
		server.use(
			http.get("/api/v1/repositories/repo-1/checks", () =>
				apiError(500, "internal", "boom"),
			),
		);
		const view = await checksCard();
		expect(
			await view.findByText(
				"Couldn't load check status. Check that the server is reachable, then retry.",
			),
		).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Retry loading" })).toBeVisible();
	});

	it("shows the test restore level", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs"),
					levelStatus("restore", { last: restoreRun() }),
				],
			}),
		);
		const view = await checksCard();
		await view.findByText("Test restore");
		expect(
			view.getByText(
				"Restores random files from random snapshots into a temporary folder and compares hashes.",
			),
		).toBeInTheDocument();
		const level = row(view, "Test restore");
		await userEvent.click(level.getByText("Show report"));
		for (const label of [
			"Snapshots sampled",
			"Files restored",
			"Data restored",
			"Hash mismatches",
			"Skipped",
		]) {
			expect(level.getByText(label)).toBeInTheDocument();
		}
		expect(level.getByText("12 MiB")).toBeInTheDocument();
		expect(level.queryByText("Stopped at the size limit.")).toBeNull();
	});

	it("shows a stopped test restore", async () => {
		const stopped = restoreRun({
			state: "done",
			outcome: "stopped",
			error: "not enough free space in the server data folder",
		});
		if (stopped.summary.restore) {
			stopped.summary.restore.stopped_at_limit = true;
			stopped.summary.restore.skipped = 4;
		}
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs"),
					levelStatus("restore", { last: stopped }),
				],
			}),
		);
		const view = await checksCard();
		await view.findByText("Test restore");
		const level = row(view, "Test restore");
		expect(
			level.getAllByText(
				"Stopped: not enough free space in the server data folder",
			).length,
		).toBeGreaterThan(0);
		expect(await level.findByText("Stopped at the size limit.")).toBeVisible();
		expect(view.getByRole("alert")).toHaveTextContent(
			"Test restore check found problems",
		);
	});

	it("shows test restore progress", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs"),
					levelStatus("restore", {
						running: restoreRun({
							state: "running",
							outcome: null,
							finished_at: null,
							duration_ms: null,
							progress: { phase: "restoring", done: 3, total: 20, bytes: 0 },
						}),
					}),
				],
			}),
		);
		const view = await checksCard();
		expect(await view.findByText("Restoring sample files")).toBeInTheDocument();
		expect(view.getByText("3 of 20 files")).toBeInTheDocument();
	});

	it("summarizes and saves settings", async () => {
		const current = openChecks(scheduled());
		let body: Record<string, Record<string, unknown>> | undefined;
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/settings/validate", () =>
				HttpResponse.json({ errors: [], schedules: [] }),
			),
			http.put(
				"/api/v1/repositories/repo-1/checks/settings",
				async ({ request }) => {
					body = (await request.json()) as typeof body;
					current.status = checkStatus({
						levels: [
							levelStatus("structure"),
							levelStatus("packs"),
							levelStatus("restore", {
								settings: {
									...levelStatus("restore").settings,
									enabled: false,
								},
							}),
						],
					});
					return HttpResponse.json(current.status);
				},
			),
		);
		const view = await checksCard();
		const trigger = await view.findByRole("button", { name: /^Settings/ });
		expect(trigger).toHaveTextContent(
			"Settings · structure daily · pack reading weekly · test restore monthly",
		);
		await openSettings(view);
		const save = view.getByRole("button", { name: "Save settings" });
		expect(save).toBeDisabled();
		expect(save).toHaveAttribute("data-variant", "outline");
		const restore = group(view, "Test restore");
		await userEvent.click(
			restore.getByRole("switch", { name: "Run on schedule" }),
		);
		expect(save).toBeEnabled();
		expect(save).toHaveAttribute("data-variant", "default");
		await userEvent.click(save);
		expect(
			await screen.findByText(
				"Check settings saved. They apply from the next run.",
			),
		).toBeInTheDocument();
		expect(body?.restore).toMatchObject({
			enabled: false,
			cron: "0 5 2 * *",
			files_per_run: 20,
			max_data_per_run: "1GiB",
		});
		expect(body?.packs).toMatchObject({ read_per_run: "5GiB" });
		expect(body?.structure).not.toHaveProperty("read_per_run");
		await waitFor(() =>
			expect(view.getByRole("button", { name: /^Settings/ })).toHaveTextContent(
				"test restore off",
			),
		);
	});

	it("shows field errors from the server", async () => {
		openChecks(scheduled());
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/settings/validate", () =>
				HttpResponse.json({ errors: [], schedules: [] }),
			),
			http.put("/api/v1/repositories/repo-1/checks/settings", () =>
				HttpResponse.json(
					{
						errors: [
							{ path: "packs.readPerRun", message: "bad size" },
							{
								path: "structure.schedule.cron",
								message: "That cron expression is not valid.",
							},
						],
					},
					{ status: 422 },
				),
			),
		);
		const view = await checksCard();
		await openSettings(view);
		const packs = group(view, "Pack reading");
		const read = packs.getByLabelText("Read per run");
		await userEvent.clear(read);
		await userEvent.type(read, "lots");
		await userEvent.click(view.getByRole("button", { name: "Save settings" }));
		expect(
			await packs.findByText("Use a size such as 5GiB or 500MiB."),
		).toBeInTheDocument();
		expect(packs.getByLabelText("Read per run")).toHaveAttribute(
			"aria-invalid",
			"true",
		);
		expect(
			await group(view, "Structure").findByText(
				"That cron expression is not valid.",
			),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(group(view, "Structure").getByLabelText("Cron")).toHaveFocus(),
		);
	});

	it("rejects a wrong number of files before sending", async () => {
		openChecks(scheduled());
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/settings/validate", () =>
				HttpResponse.json({ errors: [], schedules: [] }),
			),
		);
		let sent = false;
		server.use(
			http.put("/api/v1/repositories/repo-1/checks/settings", () => {
				sent = true;
				return HttpResponse.json(checkStatus());
			}),
		);
		const view = await checksCard();
		await openSettings(view);
		const files = group(view, "Test restore").getByLabelText("Files per run");
		await userEvent.clear(files);
		await userEvent.type(files, "5000");
		expect(
			await view.findByText("Enter a whole number from 1 to 1000."),
		).toBeInTheDocument();
		await userEvent.click(view.getByRole("button", { name: "Save settings" }));
		expect(files).toHaveFocus();
		expect(sent).toBe(false);
	});

	it("discards changes", async () => {
		openChecks(scheduled());
		server.use(
			http.post("/api/v1/repositories/repo-1/checks/settings/validate", () =>
				HttpResponse.json({ errors: [], schedules: [] }),
			),
		);
		const view = await checksCard();
		await openSettings(view);
		const read = group(view, "Pack reading").getByLabelText("Read per run");
		await userEvent.clear(read);
		await userEvent.type(read, "9GiB");
		expect(read).toHaveValue("9GiB");
		await userEvent.click(
			view.getByRole("button", { name: "Discard changes" }),
		);
		expect(read).toHaveValue("5GiB");
		expect(view.queryByRole("button", { name: "Discard changes" })).toBeNull();
		expect(view.getByRole("button", { name: "Save settings" })).toBeDisabled();
	});

	it("shows storage facts and the placeholder", async () => {
		openChecks(
			checkStatus({
				usage: {
					counted_at: Date.now() - 3_600_000,
					physical_bytes: 2 * GIB,
					logical_bytes: 7 * GIB,
					dedup_ratio: 3.4,
					packs: 12,
				},
			}),
		);
		const view = await checksCard();
		expect(await view.findByText("Storage used")).toBeInTheDocument();
		expect(view.getByText("2 GiB")).toBeInTheDocument();
		expect(view.getByText("Logical size")).toBeInTheDocument();
		expect(view.getByText("7 GiB")).toBeInTheDocument();
		expect(view.getByText("3.4×")).toBeInTheDocument();
		expect(view.getByText("Counted")).toBeInTheDocument();
		expect(
			view.queryByText("Calculated after the first structure check."),
		).toBeNull();
	});

	it("shows a placeholder until the bucket is counted", async () => {
		openChecks();
		const view = await checksCard();
		expect(
			await view.findByText("Calculated after the first structure check."),
		).toBeInTheDocument();
		expect(view.queryByText("Storage used")).toBeNull();
	});
});
