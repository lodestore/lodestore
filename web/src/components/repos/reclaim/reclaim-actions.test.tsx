import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { apiError } from "@/test/app";
import { card, GIB, gcCycle, gcStatus, openRepository } from "@/test/gc";
import { server } from "@/test/server";

const oldAgents = {
	kind: "old_agents" as const,
	message: "Some agents in this repository don't support cleanup yet.",
	hosts: [
		{ id: "h1", hostname: "web-1" },
		{ id: "h2", hostname: "db-2" },
	],
};

const open = async () => {
	const view = await card();
	await userEvent.click(
		await view.findByRole("button", { name: /^Settings · / }),
	);
	return view;
};

describe("scheduled cleanup switch", () => {
	it("changes only after the server answers", async () => {
		const current = openRepository();
		let release: () => void = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		let sent: { enabled: boolean } | undefined;
		server.use(
			http.put(
				"/api/v1/repositories/repo-1/gc/settings",
				async ({ request }) => {
					sent = (await request.json()) as { enabled: boolean };
					await gate;
					current.status = gcStatus({ enabled: true });
					return HttpResponse.json(current.status);
				},
			),
		);
		const view = await card();
		const toggle = await view.findByRole("switch", {
			name: "Scheduled cleanup",
		});
		expect(toggle).not.toBeChecked();
		await userEvent.click(toggle);
		await waitFor(() => expect(toggle).toHaveAttribute("aria-busy", "true"));
		expect(toggle).not.toBeChecked();
		release();
		await waitFor(() => expect(toggle).toBeChecked());
		expect(sent?.enabled).toBe(true);
		expect(toggle).not.toHaveAttribute("aria-busy");
		expect(view.getByRole("button", { name: "Run cleanup" })).toBeEnabled();
	});

	it("turns back off and lists the blockers on 409", async () => {
		const current = openRepository();
		server.use(
			http.put("/api/v1/repositories/repo-1/gc/settings", () => {
				current.status = gcStatus({
					preflight: {
						problems: [oldAgents],
						dry_run_allowed: true,
						checked_at: Date.now(),
					},
				});
				return HttpResponse.json(
					{ message: "Cleanup can't run yet.", problems: [oldAgents] },
					{ status: 409 },
				);
			}),
		);
		const view = await card();
		const toggle = await view.findByRole("switch", {
			name: "Scheduled cleanup",
		});
		await userEvent.click(toggle);
		expect(await view.findByText("Cleanup can't run yet")).toBeInTheDocument();
		const row = view
			.getAllByRole("listitem")
			.find((item) => item.textContent?.includes("web-1"));
		expect(row).toHaveTextContent(
			"2 hosts run an agent that does not support cleanup: web-1, db-2. Update them or revoke them.",
		);
		expect(view.getByRole("link", { name: "web-1" })).toHaveAttribute(
			"href",
			"/hosts/h1",
		);
		expect(toggle).not.toBeChecked();
		expect(view.getByRole("button", { name: "Dry run" })).toBeEnabled();
		expect(
			screen.queryByText(/Couldn't turn cleanup on/),
		).not.toBeInTheDocument();
	});

	it("turns cleanup off without checking a pause the floor has outgrown", async () => {
		const current = openRepository(
			gcStatus({ enabled: true, pause_floor: "48h" }),
		);
		let sent: { enabled: boolean; pause: string } | undefined;
		let validated = false;
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/settings/validate", () => {
				validated = true;
				return HttpResponse.json({
					pause_floor: "48h",
					problems: [],
					schedule: null,
				});
			}),
			http.put(
				"/api/v1/repositories/repo-1/gc/settings",
				async ({ request }) => {
					sent = (await request.json()) as { enabled: boolean; pause: string };
					current.status = gcStatus({ enabled: false, pause_floor: "48h" });
					return HttpResponse.json(current.status);
				},
			),
		);
		const view = await card();
		const toggle = await view.findByRole("switch", {
			name: "Scheduled cleanup",
		});
		await userEvent.click(toggle);
		await waitFor(() => expect(toggle).not.toBeChecked());
		expect(sent).toMatchObject({ enabled: false, pause: "24h" });
		expect(validated).toBe(false);
	});

	it("says why the server refused the switch", async () => {
		openRepository(gcStatus({ enabled: true }));
		server.use(
			http.put("/api/v1/repositories/repo-1/gc/settings", () =>
				HttpResponse.json(
					{
						errors: [
							{
								path: "spec.pause",
								message: "Use a duration of 48h or more.",
							},
						],
					},
					{ status: 422 },
				),
			),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("switch", { name: "Scheduled cleanup" }),
		);
		expect(
			await screen.findByText(
				"Couldn't turn cleanup off. Use a duration of 48h or more. Try again.",
			),
		).toBeInTheDocument();
	});

	it("shows the server's own words for a foreign cycle that can't be read", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				preflight: {
					problems: [
						{
							kind: "foreign_cycle",
							message:
								"locks/gc-ab12.json can't be read, so another cleanup may be marked in this bucket.",
						},
						{
							kind: "unreadable",
							message: "The repository can't be opened: timeout",
						},
					],
					dry_run_allowed: false,
					checked_at: Date.now(),
				},
			}),
		);
		const view = await card();
		await view.findByText("Cleanup can't run yet");
		const rows = view.getAllByRole("listitem").map((item) => item.textContent);
		expect(rows).toContain(
			"locks/gc-ab12.json can't be read, so another cleanup may be marked in this bucket.",
		);
		expect(rows).toContain("The repository can't be opened: timeout");
	});

	it("disables run while the checks report a problem", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				preflight: {
					problems: [
						{ kind: "server_key", message: "x", hosts: [] },
						{ kind: "agent_key", message: "y", hosts: [] },
					],
					dry_run_allowed: true,
					checked_at: Date.now(),
				},
			}),
		);
		const view = await card();
		expect(
			await view.findByText(/The server's storage key cannot delete objects/),
		).toBeInTheDocument();
		expect(view.getByRole("link", { name: "Open agent key" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#agent-key",
		);
		expect(view.getByRole("button", { name: "Run cleanup" })).toBeDisabled();
		expect(view.getByRole("button", { name: "Dry run" })).toBeEnabled();
	});
});

describe("cleanup settings", () => {
	it("summarises the saved values in the header", async () => {
		openRepository(
			gcStatus({
				schedule: {
					summary: "Every Sunday at 03:00",
					timezone: "UTC",
					next_runs: [],
					jitter_secs: 0,
					catch_up_secs: 0,
					dst_notes: [],
				},
			}),
		);
		const view = await card();
		expect(
			await view.findByRole("button", {
				name: "Settings · Every Sunday at 03:00 · pause 24h · repack below 50%",
			}),
		).toHaveAttribute("aria-expanded", "false");
	});

	it("shows the pause the next cycle uses when a policy raised the floor", async () => {
		openRepository(gcStatus({ pause_floor: "31h", pause_effective: "31h" }));
		const view = await card();
		expect(
			await view.findByRole("button", { name: /· pause 31h ·/ }),
		).toBeInTheDocument();
	});

	it("says manual only without a cron", async () => {
		openRepository(
			gcStatus({ settings: { ...gcStatus().settings, cron: "" } }),
		);
		const view = await card();
		expect(
			await view.findByRole("button", {
				name: "Settings · manual only · pause 24h · repack below 50%",
			}),
		).toBeInTheDocument();
	});

	it("shows a server error under the field and blocks saving", async () => {
		openRepository();
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/settings/validate", () =>
				HttpResponse.json({
					pause_floor: "24h",
					problems: [
						{
							path: "spec.pause",
							message: "Use a duration of 24h or more, such as 24h or 2d.",
						},
					],
					schedule: null,
				}),
			),
		);
		const view = await open();
		const pause = view.getByLabelText("Pause before deletion");
		await userEvent.clear(pause);
		await userEvent.type(pause, "12h");
		expect(
			await view.findByText(
				"Use a duration of 24h or more, such as 24h or 2d.",
			),
		).toBeInTheDocument();
		expect(pause).toHaveAttribute("aria-invalid", "true");
		expect(pause).toHaveAccessibleDescription(
			/Use a duration of 24h or more[\s\S]*At least 24h/,
		);
		expect(view.getByRole("button", { name: "Save settings" })).toBeDisabled();
	});

	it("rejects a repack percentage outside 1 to 99 without asking the server", async () => {
		openRepository();
		const view = await open();
		const pct = view.getByLabelText("Repack below");
		await userEvent.clear(pct);
		await userEvent.type(pct, "150");
		expect(
			await view.findByText("Enter a whole number from 1 to 99."),
		).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Save settings" })).toBeDisabled();
	});

	it("saves changes and tells they apply from the next cycle", async () => {
		const current = openRepository();
		let saved: Record<string, unknown> | undefined;
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/settings/validate", () =>
				HttpResponse.json({ pause_floor: "24h", problems: [], schedule: null }),
			),
			http.put(
				"/api/v1/repositories/repo-1/gc/settings",
				async ({ request }) => {
					saved = (await request.json()) as Record<string, unknown>;
					current.status = gcStatus({
						settings: { ...gcStatus().settings, pause: "2d" },
					});
					return HttpResponse.json(current.status);
				},
			),
		);
		const view = await open();
		expect(view.getByRole("button", { name: "Save settings" })).toHaveAttribute(
			"data-variant",
			"outline",
		);
		expect(view.queryByRole("button", { name: "Discard changes" })).toBeNull();
		const pause = view.getByLabelText("Pause before deletion");
		await userEvent.clear(pause);
		await userEvent.type(pause, "2d");
		const save = view.getByRole("button", { name: "Save settings" });
		expect(save).toHaveAttribute("data-variant", "default");
		expect(view.getByRole("button", { name: "Discard changes" })).toBeVisible();
		await userEvent.click(save);
		expect(
			await screen.findByText(
				"Cleanup settings saved. They apply from the next cycle.",
			),
		).toBeInTheDocument();
		expect(saved).toMatchObject({ pause: "2d", enabled: false });
		await waitFor(() =>
			expect(
				view.queryByRole("button", { name: "Discard changes" }),
			).toBeNull(),
		);
		expect(view.getByLabelText("Pause before deletion")).toHaveValue("2d");
	});

	it("tells why a save was refused by the safety checks", async () => {
		openRepository(gcStatus({ enabled: true }));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/settings/validate", () =>
				HttpResponse.json({ pause_floor: "24h", problems: [], schedule: null }),
			),
			http.put("/api/v1/repositories/repo-1/gc/settings", () =>
				HttpResponse.json(
					{ message: "Cleanup can't run yet.", problems: [oldAgents] },
					{ status: 409 },
				),
			),
		);
		const view = await open();
		await userEvent.type(view.getByLabelText("Pause before deletion"), "0");
		await userEvent.click(
			await view.findByRole("button", { name: "Save settings" }),
		);
		expect(
			await screen.findByText(
				"Couldn't save the cleanup settings. Cleanup can't run yet. Try again.",
			),
		).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Save settings" })).toBeEnabled();
	});

	it("discards edits", async () => {
		openRepository();
		const view = await open();
		const pause = view.getByLabelText("Pause before deletion");
		await userEvent.type(pause, "0");
		await userEvent.click(
			view.getByRole("button", { name: "Discard changes" }),
		);
		expect(view.getByLabelText("Pause before deletion")).toHaveValue("24h");
	});

	it("leaves jitter and catch up out of the cleanup schedule", async () => {
		openRepository();
		const view = await open();
		expect(view.getByLabelText("Cron")).toBeInTheDocument();
		expect(view.queryByText(/^Options · /)).not.toBeInTheDocument();
		expect(view.queryByText("Jitter")).not.toBeInTheDocument();
	});
});

describe("run and abort", () => {
	it("run cleanup starts a cycle and shows its phase", async () => {
		const current = openRepository(gcStatus({ enabled: true }));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/run", () => {
				current.status = gcStatus({
					enabled: true,
					cycle: gcCycle({
						state: "marking",
						progress: { done: 3, total: 10, bytes: 0 },
					}),
				});
				return HttpResponse.json({ cycle_id: "cycle-2" }, { status: 202 });
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Run cleanup" }),
		);
		expect(await view.findByText("Checking snapshots")).toBeInTheDocument();
		expect(view.getByText("3 of 10 snapshots")).toBeInTheDocument();
		expect(view.getByRole("progressbar")).toHaveAttribute(
			"aria-valuenow",
			"30",
		);
		expect(view.getByRole("button", { name: "Run cleanup" })).toHaveAttribute(
			"aria-disabled",
			"true",
		);
		expect(view.getByRole("button", { name: "Abort cycle" })).toBeVisible();
	});

	it("shows the server message when a cycle is already running", async () => {
		openRepository(gcStatus({ enabled: true }));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/run", () =>
				apiError(
					409,
					"conflict",
					"A cleanup cycle is already running in this repository.",
				),
			),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Run cleanup" }),
		);
		expect(
			await view.findByText(
				"Couldn't start cleanup. A cleanup cycle is already running in this repository. Try again.",
			),
		).toBeInTheDocument();
	});

	it("explains a resume the old agents block", async () => {
		openRepository(
			gcStatus({
				last_failure: {
					cycle_id: "cycle-1",
					origin: "schedule",
					error: "The bucket refused the request",
					after_delete: true,
					failed_at: Date.now(),
				},
			}),
		);
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/run", () =>
				HttpResponse.json(
					{
						message:
							"Some agents in this repository don't support cleanup yet.",
						problems: [oldAgents],
					},
					{ status: 409 },
				),
			),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Run cleanup" }),
		);
		expect(
			await view.findByText(
				"Couldn't start cleanup. Some agents in this repository don't support cleanup yet. Try again.",
			),
		).toBeInTheDocument();
		expect(view.getByText("Cleanup didn't start")).toBeInTheDocument();
	});

	it("sends one start per click even before the new cycle is loaded", async () => {
		const current = openRepository(gcStatus({ enabled: true }));
		let starts = 0;
		let release: () => void = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/run", () => {
				starts += 1;
				current.status = gcStatus({
					enabled: true,
					cycle: gcCycle({ state: "marking" }),
				});
				return HttpResponse.json({ cycle_id: "cycle-2" }, { status: 202 });
			}),
			http.get("/api/v1/repositories/repo-1/gc", async () => {
				if (starts > 0) await gate;
				return HttpResponse.json(current.status);
			}),
		);
		const view = await card();
		const run = await view.findByRole("button", { name: "Run cleanup" });
		await userEvent.click(run);
		await waitFor(() => expect(starts).toBe(1));
		await userEvent.click(run);
		expect(starts).toBe(1);
		release();
		expect(await view.findByText("Checking snapshots")).toBeInTheDocument();
		expect(starts).toBe(1);
	});

	it("drops a start error once the status shows a cycle", async () => {
		const current = openRepository(gcStatus({ enabled: true }));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/run", () => {
				current.status = gcStatus({
					enabled: true,
					cycle: gcCycle({ state: "marking" }),
				});
				return apiError(
					409,
					"conflict",
					"A cleanup cycle is already running in this repository.",
				);
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Run cleanup" }),
		);
		expect(await view.findByText("Checking snapshots")).toBeInTheDocument();
		expect(view.queryByText(/Couldn't start cleanup/)).toBeNull();
	});

	it("starts a dry run while a real cycle waits, but only one at a time", async () => {
		const waiting = gcCycle({
			state: "waiting",
			marked_packs: 2,
			marked_bytes: GIB,
			delete_after: Date.now() + 3_600_000,
		});
		const current = openRepository(gcStatus({ enabled: true, cycle: waiting }));
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/dry-run", () => {
				current.status = gcStatus({
					enabled: true,
					cycle: waiting,
					active_dry_run: gcCycle({ id: "dry-1", dry_run: true }),
				});
				return HttpResponse.json({ cycle_id: "dry-1" }, { status: 202 });
			}),
		);
		const view = await card();
		const dry = await view.findByRole("button", { name: "Dry run" });
		expect(dry).not.toHaveAttribute("aria-disabled");
		await userEvent.click(dry);
		expect(
			await view.findByText("A dry run is also in progress."),
		).toBeInTheDocument();
		expect(view.getByText("Marked for deletion")).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Dry run" })).toHaveAttribute(
			"aria-disabled",
			"true",
		);
	});

	it("abort cycle returns to idle and says so", async () => {
		const current = openRepository(
			gcStatus({
				enabled: true,
				cycle: gcCycle({
					state: "waiting",
					marked_packs: 4,
					marked_bytes: 2 * GIB,
					delete_after: Date.now() + 20 * 3_600_000,
				}),
			}),
		);
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/abort", () => {
				current.status = gcStatus({
					enabled: true,
					last_aborted_at: Date.now(),
				});
				return HttpResponse.json(current.status);
			}),
		);
		const view = await card();
		expect(await view.findByText("Marked for deletion")).toBeInTheDocument();
		expect(
			view.getByText(/4 packs \(2 GiB\) will be deleted after/),
		).toBeInTheDocument();
		await userEvent.click(view.getByRole("button", { name: "Abort cycle" }));
		expect(await view.findByText("Idle")).toBeInTheDocument();
		expect(view.getByText(/The last cycle was aborted/)).toHaveTextContent(
			"Nothing was deleted.",
		);
		expect(view.queryByRole("button", { name: "Abort cycle" })).toBeNull();
	});

	it("offers no abort while deleting", async () => {
		openRepository(
			gcStatus({
				enabled: true,
				cycle: gcCycle({
					state: "sweeping",
					progress: { done: 5, total: 20, bytes: 0 },
				}),
			}),
		);
		const view = await card();
		expect(await view.findByText("Deleting unused data")).toBeInTheDocument();
		expect(view.getByText("This step can't be aborted.")).toBeInTheDocument();
		expect(view.queryByRole("button", { name: "Abort cycle" })).toBeNull();
	});
});
