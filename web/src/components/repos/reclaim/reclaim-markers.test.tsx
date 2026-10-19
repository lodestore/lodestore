import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	GcEventDto,
	GcFailureDto,
	WriterMarkerDto,
} from "@/api/gen/model";
import { ReclaimStatus } from "@/components/repos/reclaim/reclaim-card";
import {
	ACTIVE_POLL_MS,
	gcPollMs,
	IDLE_POLL_MS,
	WAITING_POLL_MS,
} from "@/components/repos/reclaim/use-gc-status";
import { apiError } from "@/test/app";
import { card, gcCycle, gcStatus, openRepository } from "@/test/gc";
import { server } from "@/test/server";

const markers = (n: number): WriterMarkerDto[] =>
	Array.from({ length: n }, (_, i) => ({
		key: `locks/writer-${i}`,
		host: `host-${i + 1}`,
		started_at: Date.now() - (i + 1) * 3_600_000,
	}));

const postponed = (n: number) =>
	gcStatus({
		enabled: true,
		cycle: gcCycle({
			state: "waiting",
			marked_packs: 3,
			marked_bytes: 1024 ** 3,
			delete_after: Date.now() - 1000,
			blocked_by_writers: markers(n),
		}),
	});

const event = (seq: number, over: Partial<GcEventDto> = {}): GcEventDto => ({
	seq,
	at: Date.now() - (100 - seq) * 60_000,
	kind: "info",
	level: "info",
	message: `Event ${seq}`,
	...over,
});

describe("writer markers", () => {
	it("lists five markers and counts the rest", async () => {
		openRepository(postponed(7));
		const view = await card();
		expect(
			await view.findByText(
				"Deletion is waiting for a backup that did not finish",
			),
		).toBeInTheDocument();
		expect(
			view.getByText(
				"Cleanup does not delete anything until these backups finish or you unlock them.",
			),
		).toBeInTheDocument();
		expect(view.getByText("host-5")).toBeInTheDocument();
		expect(view.queryByText("host-6")).not.toBeInTheDocument();
		expect(view.getByText("and 2 more")).toBeInTheDocument();
		expect(
			view.getByRole("button", { name: "Unlock 7 markers…" }),
		).toBeVisible();
		expect(view.getByText("Deletion is postponed")).toBeInTheDocument();
		expect(
			view.getByText(
				"3 packs are marked. Deletion waits for the backups listed above.",
			),
		).toBeInTheDocument();
	});

	it("names a single marker and an unreadable one", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				cycle: gcCycle({
					state: "waiting",
					blocked_by_writers: [
						{ key: "locks/writer-x", host: null, started_at: null },
					],
				}),
			}),
		);
		const view = await card();
		expect(
			await view.findByRole("button", { name: "Unlock marker…" }),
		).toBeVisible();
		expect(view.getByText("Unknown host")).toBeInTheDocument();
	});

	it("asks before unlocking, focusing the safe choice", async () => {
		openRepository(postponed(7));
		let unlocked = 0;
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/unlock", () => {
				unlocked += 1;
				return HttpResponse.json({ unlocked: 7 });
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Unlock 7 markers…" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(within(dialog).getByText("Unlock 7 markers?")).toBeInTheDocument();
		expect(
			within(dialog).getByText(/Cleanup stops waiting for: host-1, host-2/),
		).toHaveTextContent(
			"If one of these backups is still running, its snapshot can lose data and fail to restore. Unlock only backups you know were interrupted.",
		);
		expect(
			within(dialog).getByRole("button", { name: "Keep waiting" }),
		).toHaveFocus();
		expect(unlocked).toBe(0);
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Keep waiting" }),
		);
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
		expect(unlocked).toBe(0);
	});

	it("names at most ten hosts in the dialog", async () => {
		openRepository(postponed(12));
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Unlock 12 markers…" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		const body = within(dialog).getByText(/Cleanup stops waiting for:/);
		expect(body).toHaveTextContent("host-10 and 2 more.");
		expect(body).not.toHaveTextContent("host-11");
	});

	it("unlock markers confirms, closes and tells how many", async () => {
		const current = openRepository(postponed(7));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/unlock", () => {
				current.status = gcStatus({
					enabled: true,
					cycle: gcCycle({ state: "waiting", delete_after: Date.now() - 1000 }),
				});
				return HttpResponse.json({ unlocked: 7 });
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Unlock 7 markers…" }),
		);
		await userEvent.click(
			within(await screen.findByRole("alertdialog")).getByRole("button", {
				name: "Unlock markers",
			}),
		);
		expect(
			await screen.findByText("Unlocked 7 markers. Cleanup continues."),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
		await waitFor(() =>
			expect(
				view.queryByText(
					"Deletion is waiting for a backup that did not finish",
				),
			).not.toBeInTheDocument(),
		);
		expect(document.getElementById("reclaim-status")).toHaveFocus();
	});

	it("keeps the dialog open and explains a failure", async () => {
		openRepository(postponed(2));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/unlock", () =>
				apiError(500, "internal", "The bucket did not answer."),
			),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Unlock 2 markers…" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Unlock markers" }),
		);
		expect(
			await within(dialog).findByText(
				"Couldn't unlock the markers. The bucket did not answer. Try again.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("alertdialog")).toBeInTheDocument();
	});

	it("closes the dialog and refreshes when the markers are already gone", async () => {
		const current = openRepository(postponed(2));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/unlock", () => {
				current.status = gcStatus({
					enabled: true,
					cycle: gcCycle({ state: "waiting", delete_after: Date.now() - 1000 }),
				});
				return apiError(409, "conflict", "There are no markers to unlock.");
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Unlock 2 markers…" }),
		);
		await userEvent.click(
			within(await screen.findByRole("alertdialog")).getByRole("button", {
				name: "Unlock markers",
			}),
		);
		expect(
			await screen.findByText(
				"Couldn't unlock the markers. There are no markers to unlock. Try again.",
			),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
		await waitFor(() =>
			expect(
				view.queryByText(
					"Deletion is waiting for a backup that did not finish",
				),
			).not.toBeInTheDocument(),
		);
	});
});

const failure = (over: Partial<GcFailureDto> = {}): GcFailureDto => ({
	cycle_id: "cycle-1",
	origin: "schedule",
	error: "The snapshot snapshots/ab12 can't be read",
	after_delete: false,
	failed_at: Date.now() - 60_000,
	...over,
});

describe("failed cycles", () => {
	it("says nothing was deleted when it failed before deleting", async () => {
		openRepository(gcStatus({ enabled: true, last_failure: failure() }));
		const view = await card();
		const alert = (await view.findByText("Cleanup stopped")).closest(
			"[role=alert]",
		);
		expect(alert).toHaveTextContent(
			"Nothing was deleted. The snapshot snapshots/ab12 can't be read.",
		);
		expect(
			view.getByText("Fix the cause, then run a dry run to check."),
		).toBeInTheDocument();
		expect(view.getByText("Idle")).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Dry run" })).toBeEnabled();
	});

	it("says it continues when it failed while deleting", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				last_failure: failure({
					error: "The bucket refused the request",
					after_delete: true,
				}),
			}),
		);
		const view = await card();
		expect(
			await view.findByText("Cleanup stopped while deleting"),
		).toBeInTheDocument();
		expect(
			view.getByText(
				"The bucket refused the request. It continues where it stopped, on the next run or when the server retries it. Nothing a snapshot needs was removed.",
			),
		).toBeInTheDocument();
	});

	it("lets a stopped deletion resume with cleanup off or a blocker listed", async () => {
		openRepository(
			gcStatus({
				enabled: false,
				last_failure: failure({ after_delete: true }),
				preflight: {
					problems: [{ kind: "server_key", message: "x", hosts: [] }],
					dry_run_allowed: true,
					checked_at: Date.now(),
				},
			}),
		);
		const view = await card();
		await view.findByText("Cleanup stopped while deleting");
		expect(view.getByRole("button", { name: "Run cleanup" })).toBeEnabled();
		expect(view.queryByText(/Turn on scheduled cleanup/)).toBeNull();
		expect(
			view.queryByText("Nothing is deleted from this repository."),
		).toBeNull();
	});

	it("keeps Run off while a failure happened before deleting and cleanup is off", async () => {
		openRepository(gcStatus({ enabled: false, last_failure: failure() }));
		const view = await card();
		await view.findByText("Cleanup stopped");
		expect(view.getByRole("button", { name: "Run cleanup" })).toBeDisabled();
	});

	it("shows the failure beside a dry run that is running", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				last_failure: failure({ after_delete: true }),
				cycle: gcCycle({ dry_run: true, state: "marking" }),
				active_dry_run: gcCycle({ dry_run: true, state: "marking" }),
			}),
		);
		const view = await card();
		expect(
			await view.findByText("Cleanup stopped while deleting"),
		).toBeInTheDocument();
		expect(view.getByText("Dry run in progress")).toBeInTheDocument();
	});

	it("drops the failure once a later cleanup has started", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				cycle: gcCycle({ state: "marking" }),
				last_failure: null,
			}),
		);
		const view = await card();
		expect(await view.findByText("Checking snapshots")).toBeInTheDocument();
		expect(view.queryByText("Cleanup stopped")).not.toBeInTheDocument();
	});
});

describe("cycle log", () => {
	it("shows twenty entries and counts the rest", async () => {
		openRepository(
			gcStatus({
				events: Array.from({ length: 25 }, (_, i) => event(i + 1)),
				events_total: 25,
			}),
		);
		const view = await card();
		const trigger = await view.findByRole("button", { name: "Cycle log" });
		expect(trigger).toHaveAttribute("aria-expanded", "false");
		await userEvent.click(trigger);
		expect(view.getAllByText(/^Event \d+$/)).toHaveLength(20);
		expect(view.getByText("Event 25")).toBeInTheDocument();
		expect(view.queryByText("Event 5")).not.toBeInTheDocument();
		expect(view.getByText("and 5 more")).toBeInTheDocument();
	});

	it("is hidden when no cycle ever ran", async () => {
		openRepository();
		const view = await card();
		await view.findByText("Cleanup is off");
		expect(view.queryByText("Cycle log")).not.toBeInTheDocument();
	});
});

describe("status updates", () => {
	it("polls every 2 s while working, 15 s while waiting and 30 s otherwise", () => {
		const at = (state: Parameters<typeof gcCycle>[0], dry = false) =>
			gcPollMs(gcStatus({ cycle: gcCycle({ ...state, dry_run: dry }) }));
		expect(gcPollMs(undefined)).toBe(IDLE_POLL_MS);
		expect(gcPollMs(gcStatus())).toBe(30_000);
		for (const state of [
			"marking",
			"repacking",
			"verifying",
			"sweeping",
		] as const) {
			expect(at({ state })).toBe(ACTIVE_POLL_MS);
		}
		expect(ACTIVE_POLL_MS).toBe(2_000);
		expect(at({ state: "waiting" })).toBe(WAITING_POLL_MS);
		expect(WAITING_POLL_MS).toBe(15_000);
		expect(at({ state: "marking" }, true)).toBe(ACTIVE_POLL_MS);
	});

	it("announces a new phase but not progress ticks", () => {
		const status = (state: "repacking" | "sweeping", done: number) =>
			gcStatus({
				enabled: true,
				cycle: gcCycle({
					state,
					progress: { done, total: 40, bytes: done * 1024 ** 2 },
				}),
			});
		const { rerender } = render(
			<ReclaimStatus status={status("repacking", 3)} />,
		);
		const region = screen.getByRole("status");
		expect(region).toHaveTextContent("Rewriting packs");
		expect(screen.getByText(/3 of 40 packs/)).toBeInTheDocument();
		expect(region).not.toContainElement(screen.getByText(/3 of 40 packs/));
		const before = region.textContent;
		rerender(<ReclaimStatus status={status("repacking", 9)} />);
		expect(screen.getByText(/9 of 40 packs/)).toBeInTheDocument();
		expect(screen.getByRole("status").textContent).toBe(before);
		rerender(<ReclaimStatus status={status("sweeping", 1)} />);
		expect(screen.getByRole("status")).toHaveTextContent(
			"Deleting unused data",
		);
		expect(screen.getByRole("status")).toHaveAttribute("aria-live", "polite");
	});

	it("shows the pause and the delete time while waiting", () => {
		render(
			<ReclaimStatus
				status={gcStatus({
					enabled: true,
					cycle: gcCycle({
						state: "waiting",
						marked_packs: 1,
						marked_bytes: 2048,
						delete_after: Date.now() + 12 * 3_600_000,
					}),
				})}
			/>,
		);
		expect(screen.getByRole("status")).toHaveTextContent(
			"Marked for deletion1 pack (2 KiB) will be deleted after",
		);
		expect(screen.getByRole("progressbar")).toHaveAttribute(
			"aria-label",
			"Pause before deletion",
		);
		expect(
			screen.getByText("The pause lets running backups finish first."),
		).toBeInTheDocument();
	});

	it("measures the pause from the cycle, not from the current setting", () => {
		render(
			<ReclaimStatus
				status={gcStatus({
					enabled: true,
					settings: { ...gcStatus().settings, pause: "7d" },
					cycle: gcCycle({
						state: "waiting",
						started_at: Date.now() - 12 * 3_600_000,
						delete_after: Date.now() + 12 * 3_600_000,
					}),
				})}
			/>,
		);
		expect(screen.getByRole("progressbar")).toHaveAttribute(
			"aria-valuenow",
			"50",
		);
	});

	it("shows no deadline or bar while the delete time is unknown", () => {
		render(
			<ReclaimStatus
				status={gcStatus({
					enabled: true,
					cycle: gcCycle({ state: "waiting", delete_after: null }),
				})}
			/>,
		);
		expect(screen.getByRole("status")).toHaveTextContent("Marked for deletion");
		expect(screen.queryByRole("progressbar")).toBeNull();
		expect(screen.queryByText(/will be deleted after/)).toBeNull();
	});
});
