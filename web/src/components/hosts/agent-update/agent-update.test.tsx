import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	AgentUpdateStatus,
	Host,
	HostDetail,
	HostUpdate,
	Rollout,
} from "@/api/gen/model";
import {
	agentUpdateStatus,
	hostUpdate,
	mockAgentUpdates,
	rollback,
	rollout,
	wave,
} from "@/test/agent-update";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;
const hid = (name: string) => `${name}-0000-4000-8000-000000000000`;

function makeHost(name: string, over: Partial<Host> = {}): Host {
	return {
		id: hid(name),
		hostname: name,
		os: "linux",
		arch: "x86_64",
		distro: null,
		version: "0.1.0",
		status: "online",
		labels: [],
		repository_id: "repo-1",
		enrolled_at: Date.now() - 30 * DAY,
		last_seen_at: Date.now() - 20_000,
		cert_not_after: Date.now() + 60 * DAY,
		...over,
	};
}

const upd = (name: string, over: Partial<HostUpdate> = {}) =>
	hostUpdate(hid(name), over);
const outdated = (name: string, over: Partial<HostUpdate> = {}) =>
	upd(name, { outdated: true, updatable: true, ...over });

async function openHosts(
	hosts: Host[],
	status: AgentUpdateStatus | (() => AgentUpdateStatus),
) {
	server.use(http.get("/api/v1/hosts", () => HttpResponse.json(hosts)));
	mockAgentUpdates(status);
	useSignedIn({ info: { version: "0.2.0" } });
	const app = renderApp("/hosts");
	await screen.findByRole("heading", { name: "Hosts" });
	await screen.findByText(hosts[0].hostname);
	return app;
}

const fleet = () => [
	makeHost("web-1"),
	makeHost("web-2"),
	makeHost("db-1", { version: "0.2.0" }),
];
const fleetStatus = () =>
	agentUpdateStatus([
		outdated("web-1"),
		outdated("web-2"),
		upd("db-1", { version: "0.2.0" }),
	]);

describe("agent update in the hosts table", () => {
	it("shows the Outdated chip, state lines and checkboxes only for outdated hosts", async () => {
		await openHosts(fleet(), fleetStatus());
		expect(
			await screen.findByRole("button", { name: "Outdated · 2" }),
		).toBeInTheDocument();
		expect(screen.getAllByText("Outdated", { selector: "span" })).toHaveLength(
			2,
		);
		expect(
			await screen.findByRole("checkbox", { name: "Select web-1" }),
		).toBeInTheDocument();
		expect(
			screen.getByRole("checkbox", { name: "Select web-2" }),
		).toBeInTheDocument();
		expect(
			screen.queryByRole("checkbox", { name: "Select db-1" }),
		).not.toBeInTheDocument();
	});

	it("hides the chip when nothing is outdated", async () => {
		await openHosts(
			[makeHost("db-1", { version: "0.2.0" })],
			agentUpdateStatus([upd("db-1", { version: "0.2.0", state: "queued" })]),
		);
		await screen.findByText("Queued");
		expect(
			screen.queryByRole("button", { name: /^Outdated/ }),
		).not.toBeInTheDocument();
	});

	it("filters the table to outdated hosts with the chip", async () => {
		await openHosts(fleet(), fleetStatus());
		await userEvent.click(
			await screen.findByRole("button", { name: "Outdated · 2" }),
		);
		const table = screen.getByRole("table");
		expect(within(table).getByText("web-1")).toBeInTheDocument();
		expect(within(table).getByText("web-2")).toBeInTheDocument();
		expect(within(table).queryByText("db-1")).not.toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Outdated · 2" }),
		).toHaveAttribute("aria-pressed", "true");
	});

	it("selects and clears all outdated hosts from the header checkbox", async () => {
		await openHosts(fleet(), fleetStatus());
		const all = await screen.findByRole("checkbox", {
			name: "Select all outdated hosts",
		});
		await userEvent.click(all);
		expect(
			screen.getByRole("checkbox", { name: "Select web-1" }),
		).toBeChecked();
		expect(
			screen.getByRole("checkbox", { name: "Select web-2" }),
		).toBeChecked();
		await userEvent.click(all);
		expect(
			screen.getByRole("checkbox", { name: "Select web-1" }),
		).not.toBeChecked();
	});

	it("keeps a host that cannot update itself without a checkbox and explains why", async () => {
		await openHosts(
			[makeHost("old-1")],
			agentUpdateStatus(
				[
					outdated("old-1", {
						updatable: false,
						blocker: "no_self_update",
					}),
				],
				{ signed: false },
			),
		);
		const line = await screen.findByText("Outdated", { selector: "span" });
		expect(line).toHaveAttribute(
			"title",
			"This agent can't update itself. Run install.sh on the host.",
		);
		expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
	});

	it("names the missing binary for a host the server has no signed agent for", async () => {
		await openHosts(
			[makeHost("arm-1", { arch: "aarch64" })],
			agentUpdateStatus([
				outdated("arm-1", {
					arch: "aarch64",
					updatable: false,
					blocker: "no_binary",
				}),
			]),
		);
		const line = await screen.findByText("Outdated", { selector: "span" });
		expect(line?.getAttribute("title")).toMatch(
			/^This server has no signed agent 0\.2\.0 for aarch64\./,
		);
	});

	it("shows the update state under the version", async () => {
		await openHosts(
			[
				makeHost("q"),
				makeHost("w"),
				makeHost("o"),
				makeHost("u"),
				makeHost("r"),
			],
			agentUpdateStatus([
				upd("q", { outdated: true, state: "queued" }),
				upd("w", { outdated: true, state: "waiting_for_job" }),
				upd("o", { outdated: true, state: "offline_queued" }),
				upd("u", {
					outdated: true,
					state: "updating",
					target_version: "0.2.0",
				}),
				upd("r", {
					outdated: true,
					state: "rolled_back",
					reason: "New version did not connect within 2 min.",
					update_id: "job-9",
				}),
			]),
		);
		expect(await screen.findByText("Queued")).toBeInTheDocument();
		expect(screen.getByText("Waiting for job")).toBeInTheDocument();
		expect(screen.getByText("Offline, queued")).toBeInTheDocument();
		expect(screen.getByText("Updating to 0.2.0")).toBeInTheDocument();
		const link = screen.getByRole("link", { name: "Update rolled back" });
		expect(link).toHaveAttribute("href", "/jobs/job-9");
		await waitFor(() =>
			expect(link.closest("[title]")).toHaveAttribute(
				"title",
				"New version did not connect within 2 min.",
			),
		);
		expect(screen.queryByText("Outdated", { selector: "span" })).toBeNull();
	});
});

describe("the target of a running update", () => {
	it("names the version the host is going to, not the newest build", async () => {
		await openHosts(
			[makeHost("u")],
			agentUpdateStatus(
				[
					upd("u", {
						outdated: true,
						state: "updating",
						target_version: "0.2.0",
					}),
				],
				{ target_version: "0.3.0" },
			),
		);
		expect(await screen.findByText("Updating to 0.2.0")).toBeInTheDocument();
	});
});

describe("starting an agent update", () => {
	function captureStart(status = 201) {
		const bodies: unknown[] = [];
		server.use(
			http.post("/api/v1/agent-updates", async ({ request }) => {
				bodies.push(await request.json());
				return status === 201
					? HttpResponse.json(
							{ id: "rollout-1", hosts: 2, skipped: 0 },
							{ status },
						)
					: HttpResponse.json(
							{ code: "conflict", message: "a rollout is already active" },
							{ status },
						);
			}),
		);
		return bodies;
	}

	it("offers one banner for every outdated agent", async () => {
		await openHosts(fleet(), fleetStatus());
		const banner = await screen.findByText("2 agents run an older version.");
		expect(banner.parentElement).toHaveTextContent(
			"This server ships agent 0.2.0, signed by the project key.",
		);
	});

	it("shows neither the banner nor the selection bar until the status loads", async () => {
		let release: () => void = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.get("/api/v1/agent-updates", async () => {
				await gate;
				return HttpResponse.json(fleetStatus());
			}),
		);
		server.use(http.get("/api/v1/hosts", () => HttpResponse.json(fleet())));
		useSignedIn({ info: { version: "0.2.0" } });
		renderApp("/hosts");
		await screen.findByText("web-1");
		expect(screen.queryByText(/run an older version/)).toBeNull();
		expect(screen.queryByRole("checkbox")).toBeNull();
		release();
		expect(
			await screen.findByText("2 agents run an older version."),
		).toBeInTheDocument();
	});

	it("shows the selection bar and clears it", async () => {
		await openHosts(fleet(), fleetStatus());
		await userEvent.click(
			await screen.findByRole("checkbox", { name: "Select web-1" }),
		);
		await userEvent.click(
			screen.getByRole("checkbox", { name: "Select web-2" }),
		);
		expect(screen.getByText("2 selected")).toBeInTheDocument();
		await userEvent.click(screen.getByRole("button", { name: "Clear" }));
		expect(screen.queryByText("2 selected")).toBeNull();
		expect(
			screen.getByRole("checkbox", { name: "Select web-1" }),
		).not.toBeChecked();
	});

	it("confirms and starts a rollout for every outdated agent", async () => {
		const bodies = captureStart();
		await openHosts(fleet(), fleetStatus());
		await userEvent.click(
			await screen.findByRole("button", { name: "Update outdated agents" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Update 2 agents to 0.2.0?",
		});
		expect(dialog).toHaveTextContent("2 canary hosts, then waves of 10");
		expect(dialog).toHaveTextContent("More than 1 rollback");
		expect(dialog).toHaveTextContent(
			"Update after their running backup or restore",
		);
		expect(dialog).toHaveTextContent(
			"goes back to its current version if the new one does not connect within 2 minutes.",
		);
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Update 2 agents" }),
		);
		await waitFor(() => expect(bodies).toEqual([{ outdated: true }]));
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
	});

	it("starts a rollout for the selected hosts only", async () => {
		const bodies = captureStart();
		await openHosts(fleet(), fleetStatus());
		await userEvent.click(
			await screen.findByRole("checkbox", { name: "Select web-2" }),
		);
		await userEvent.click(
			screen.getByRole("button", { name: "Update selected" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Update 1 agent to 0.2.0?",
		});
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Update 1 agent" }),
		);
		await waitFor(() => expect(bodies).toEqual([{ hosts: [hid("web-2")] }]));
	});

	it("lists offline hosts in the confirmation only when there are any", async () => {
		await openHosts(
			[makeHost("web-1"), makeHost("web-2", { status: "offline" })],
			agentUpdateStatus([outdated("web-1"), outdated("web-2")]),
		);
		await userEvent.click(
			await screen.findByRole("button", { name: "Update outdated agents" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(dialog).toHaveTextContent("Offline hosts");
		expect(dialog).toHaveTextContent("1 · update when they reconnect");
	});

	it("omits the offline row when every target is online", async () => {
		await openHosts(fleet(), fleetStatus());
		await userEvent.click(
			await screen.findByRole("button", { name: "Update outdated agents" }),
		);
		expect(await screen.findByRole("alertdialog")).not.toHaveTextContent(
			"Offline hosts",
		);
	});

	it("explains why agents cannot be updated when the binary is not signed", async () => {
		await openHosts(
			fleet(),
			agentUpdateStatus(
				[
					outdated("web-1", { updatable: false }),
					outdated("web-2", { updatable: false }),
				],
				{ signed: false },
			),
		);
		const alert = await screen.findByText(
			"Agents can't be updated from this server.",
		);
		expect(alert.parentElement).toHaveTextContent(
			"The agent binary in /usr/share/lodestore/agents has no .minisig signature.",
		);
		expect(screen.queryByRole("checkbox")).toBeNull();
		expect(screen.queryByRole("button", { name: /^Outdated/ })).toBeNull();
		expect(
			screen.queryByRole("button", { name: "Update outdated agents" }),
		).toBeNull();
	});

	it("reports a running update and reloads the status on 409", async () => {
		const bodies = captureStart(409);
		let loads = 0;
		await openHosts(fleet(), () => {
			loads += 1;
			return fleetStatus();
		});
		await userEvent.click(
			await screen.findByRole("button", { name: "Update outdated agents" }),
		);
		const before = loads;
		await userEvent.click(
			within(await screen.findByRole("alertdialog")).getByRole("button", {
				name: "Update 2 agents",
			}),
		);
		expect(
			await screen.findByText("An update is already running."),
		).toBeInTheDocument();
		expect(bodies).toHaveLength(1);
		await waitFor(() => expect(loads).toBeGreaterThan(before));
	});
});

describe("rollout card", () => {
	const withRollout = (over: Partial<Rollout> = {}) =>
		agentUpdateStatus([outdated("web-1"), outdated("web-2")], {
			rollout: rollout(over),
		});
	const running = () =>
		withRollout({
			total: 5,
			updated: 3,
			waves: [wave(0, 2, 2, 0, 0), wave(1, 3, 1, 0, 1)],
		});

	function capture(action: string) {
		const calls: string[] = [];
		server.use(
			http.post(`/api/v1/agent-updates/rollout-1/${action}`, ({ request }) => {
				calls.push(new URL(request.url).pathname);
				return new HttpResponse(null, { status: 204 });
			}),
		);
		return calls;
	}

	it("shows progress and steps instead of the banner while running", async () => {
		await openHosts(fleet(), running());
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("Agent update to 0.2.0");
		expect(card).toHaveTextContent("Updating · 3 of 5 hosts");
		expect(card).toHaveTextContent("Canary");
		expect(card).toHaveTextContent("2 of 2 updated");
		expect(card).toHaveTextContent("Wave 1");
		expect(card).toHaveTextContent("1 updated, 0 rolled back, 1 waiting");
		expect(
			within(card).getByRole("button", { name: "Pause" }),
		).toBeInTheDocument();
		expect(
			within(card).getByRole("button", { name: "Cancel update" }),
		).toBeInTheDocument();
		expect(screen.queryByText(/run an older version/)).toBeNull();
		expect(
			within(card).getByRole("link", { name: "Open update jobs" }),
		).toHaveAttribute("href", "/jobs?kind=agent_update");
	});

	it("pauses a running update", async () => {
		const calls = capture("pause");
		await openHosts(fleet(), running());
		await userEvent.click(await screen.findByRole("button", { name: "Pause" }));
		await waitFor(() =>
			expect(calls).toEqual(["/api/v1/agent-updates/rollout-1/pause"]),
		);
	});

	it("shows why the server paused an update", async () => {
		await openHosts(
			fleet(),
			withRollout({
				state: "paused",
				pause_reason: "No signed agent 0.2.0 for x86_64 on this server.",
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent(
			"Paused · No signed agent 0.2.0 for x86_64 on this server.",
		);
		expect(card).not.toHaveTextContent("the limit is");
	});

	it("does not blame rollbacks for a pause they did not cause", async () => {
		await openHosts(
			fleet(),
			withRollout({
				state: "paused",
				rolled_back: 3,
				rollbacks_since_resume: 1,
				pause_after: 1,
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("Paused");
		expect(card).not.toHaveTextContent("the limit is");
	});

	it("lists rollbacks with reasons and resumes a paused update", async () => {
		const calls = capture("resume");
		await openHosts(
			fleet(),
			withRollout({
				state: "paused",
				total: 6,
				updated: 3,
				rolled_back: 3,
				rollbacks_since_resume: 3,
				pause_after: 2,
				waves: [wave(0, 2, 2, 0, 0), wave(1, 4, 1, 3, 0)],
				rollbacks: [
					rollback("web-7", "New version did not connect within 2 min."),
					rollback("db-2", "Signature check failed: key not trusted."),
				],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent(
			"Paused · 3 hosts rolled back, the limit is 2",
		);
		expect(card).toHaveTextContent(
			"web-7New version did not connect within 2 min.",
		);
		expect(card).toHaveTextContent("db-2Signature check failed");
		await userEvent.click(within(card).getByRole("button", { name: "Resume" }));
		await waitFor(() =>
			expect(calls).toEqual(["/api/v1/agent-updates/rollout-1/resume"]),
		);
	});

	it("asks before cancelling and keeps updating on request", async () => {
		const calls = capture("cancel");
		await openHosts(fleet(), running());
		await userEvent.click(
			await screen.findByRole("button", { name: "Cancel update" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Cancel the update?",
		});
		expect(dialog).toHaveTextContent(
			"Hosts already updated keep 0.2.0. Queued hosts stay on their version.",
		);
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Keep updating" }),
		);
		expect(screen.queryByRole("alertdialog")).toBeNull();
		expect(calls).toHaveLength(0);
		await userEvent.click(
			screen.getByRole("button", { name: "Cancel update" }),
		);
		await userEvent.click(
			within(await screen.findByRole("alertdialog")).getByRole("button", {
				name: "Cancel update",
			}),
		);
		await waitFor(() =>
			expect(calls).toEqual(["/api/v1/agent-updates/rollout-1/cancel"]),
		);
	});

	it("dismisses a finished update that had rollbacks", async () => {
		const calls = capture("dismiss");
		await openHosts(
			fleet(),
			withRollout({
				state: "done",
				total: 5,
				updated: 4,
				rolled_back: 1,
				waves: [wave(0, 2, 2, 0, 0), wave(1, 3, 2, 1, 0)],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("Done · 4 updated, 1 rolled back");
		expect(
			within(card).queryByRole("button", { name: "Cancel update" }),
		).toBeNull();
		await userEvent.click(
			within(card).getByRole("button", { name: "Dismiss" }),
		);
		await waitFor(() =>
			expect(calls).toEqual(["/api/v1/agent-updates/rollout-1/dismiss"]),
		);
	});

	it("collapses more than three waves", async () => {
		await openHosts(
			fleet(),
			withRollout({
				total: 12,
				updated: 4,
				waves: [
					wave(0, 2, 2, 0, 0),
					wave(1, 2, 2, 0, 0),
					wave(2, 2, 0, 0, 2),
					wave(3, 2, 0, 0, 2),
					wave(4, 2, 0, 0, 2),
					wave(5, 2, 0, 0, 2),
				],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("Canary");
		expect(card).toHaveTextContent("Wave 2");
		expect(card).not.toHaveTextContent("Wave 3");
		expect(card).toHaveTextContent("4 more waves");
	});

	it("does not show cancelled hosts as queued", async () => {
		await openHosts(
			fleet(),
			withRollout({
				state: "cancelled",
				waves: [wave(0, 2, 2, 0, 0), wave(1, 3, 1, 0, 0), wave(2, 3, 0, 0, 0)],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).not.toHaveTextContent("queued");
		expect(card).toHaveTextContent("1 updated, 0 rolled back, 0 waiting");
		expect(card).toHaveTextContent("3 hosts skipped");
		expect(within(card).getByText("Wave 2")).not.toHaveClass("text-success");
	});

	it("shows the wave that is running past one with cancelled hosts", async () => {
		await openHosts(
			fleet(),
			withRollout({
				waves: [
					wave(0, 2, 2, 0, 0),
					wave(1, 3, 1, 0, 0),
					wave(2, 2, 0, 0, 2),
					wave(3, 2, 0, 0, 2),
				],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("Wave 2");
		expect(card).not.toHaveTextContent("Wave 1");
	});

	it("keeps counting a waiting wave that already has updated hosts", async () => {
		await openHosts(
			fleet(),
			withRollout({
				waves: [wave(0, 2, 0, 0, 2), wave(1, 3, 1, 0, 2)],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		expect(card).toHaveTextContent("1 updated, 0 rolled back, 2 waiting");
	});

	it("does not mark a wave of rollbacks as a success", async () => {
		await openHosts(
			fleet(),
			withRollout({
				state: "done",
				updated: 0,
				rolled_back: 2,
				waves: [wave(0, 2, 0, 2, 0)],
			}),
		);
		const card = await screen.findByRole("region", {
			name: /Agent update to/,
		});
		const step = within(card).getByText("Canary");
		expect(step).not.toHaveClass("text-success");
		expect(step).toHaveClass("text-destructive");
	});

	it("shows the server's reason when an action conflicts", async () => {
		const reason = "The update is done; it can't be paused.";
		server.use(
			http.post("/api/v1/agent-updates/rollout-1/pause", () =>
				HttpResponse.json(
					{ code: "conflict", message: reason },
					{ status: 409 },
				),
			),
		);
		await openHosts(fleet(), withRollout());
		await userEvent.click(await screen.findByRole("button", { name: "Pause" }));
		expect(await screen.findByText(reason)).toBeInTheDocument();
	});

	it("blocks the card's buttons while an action is on its way", async () => {
		let release: () => void = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.post("/api/v1/agent-updates/rollout-1/pause", async () => {
				await gate;
				return new HttpResponse(null, { status: 204 });
			}),
		);
		await openHosts(fleet(), withRollout());
		const pause = await screen.findByRole("button", { name: "Pause" });
		await userEvent.click(pause);
		await waitFor(() => expect(pause).toBeDisabled());
		expect(
			screen.getByRole("button", { name: "Cancel update" }),
		).toBeDisabled();
		release();
		await waitFor(() => expect(pause).toBeEnabled());
	});

	it("offers a retry in place of the card when the status fails to load", async () => {
		server.use(
			http.get("/api/v1/hosts", () => HttpResponse.json(fleet())),
			http.get("/api/v1/agent-updates", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		useSignedIn({ info: { version: "0.2.0" } });
		renderApp("/hosts");
		const alert = await screen.findByRole("alert");
		expect(alert).toHaveTextContent("Couldn't load the agent update status.");
		expect(
			within(alert).getByRole("button", { name: "Retry loading" }),
		).toBeInTheDocument();
	});
});

describe("agent update on the host page", () => {
	const detail = (over: Partial<HostDetail> = {}): HostDetail => ({
		...makeHost("web-1", over),
		cert_issued_at: Date.now() - 30 * DAY,
		next_renewal_at: Date.now() + 30 * DAY,
		repository_name: "main",
		kernel: null,
		uptime_s: null,
		mounts: [],
		...over,
	});

	async function openHost(
		host: HostDetail,
		update: Partial<HostUpdate>,
		status: Partial<AgentUpdateStatus> = {},
	) {
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () => HttpResponse.json(host)),
			http.get(`/api/v1/hosts/${host.id}/policies`, () =>
				HttpResponse.json([]),
			),
		);
		mockAgentUpdates(
			agentUpdateStatus(
				[hostUpdate(host.id, { hostname: "web-1", ...update })],
				status,
			),
		);
		useSignedIn({ info: { version: "0.2.0" } });
		renderApp(`/hosts/${host.id}`);
		await screen.findByText("Version");
	}

	it("updates one agent after confirming", async () => {
		const bodies: unknown[] = [];
		server.use(
			http.post("/api/v1/agent-updates", async ({ request }) => {
				bodies.push(await request.json());
				return HttpResponse.json(
					{ id: "rollout-1", hosts: 1, skipped: 0 },
					{ status: 201 },
				);
			}),
		);
		const host = detail();
		await openHost(host, { outdated: true, updatable: true });
		await userEvent.click(
			await screen.findByRole("button", { name: "Update agent" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Update web-1 to 0.2.0?",
		});
		expect(dialog).not.toHaveTextContent("canary");
		expect(dialog).not.toHaveTextContent("Offline hosts");
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Update agent" }),
		);
		await waitFor(() => expect(bodies).toEqual([{ hosts: [host.id] }]));
	});

	it("says an offline host updates when it reconnects", async () => {
		await openHost(detail({ status: "offline" }), {
			outdated: true,
			updatable: true,
		});
		await userEvent.click(
			await screen.findByRole("button", { name: "Update agent" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(dialog).toHaveTextContent("Offline hosts");
		expect(dialog).toHaveTextContent("1 · update when they reconnect");
	});

	it("shows the version the update status reports", async () => {
		await openHost(detail({ version: "0.1.0" }), { version: "0.2.0" });
		expect(await screen.findByText("0.2.0")).toBeInTheDocument();
		expect(screen.queryByText("0.1.0")).toBeNull();
	});

	it("tells an agent from before this phase to run install.sh, even without a signature", async () => {
		await openHost(
			detail(),
			{ outdated: true, updatable: false, blocker: "no_self_update" },
			{ signed: false },
		);
		expect(
			await screen.findByText(
				"This agent can't update itself. Run install.sh on the host.",
			),
		).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Update agent" })).toBeNull();
	});

	it("explains a missing signature", async () => {
		await openHost(
			detail(),
			{ outdated: true, updatable: false, blocker: "unsigned" },
			{ signed: false },
		);
		expect(
			await screen.findByText(/has no \.minisig signature/),
		).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Update agent" })).toBeNull();
	});

	it("explains a missing binary for the host architecture", async () => {
		await openHost(detail(), {
			outdated: true,
			updatable: false,
			blocker: "no_binary",
		});
		expect(
			await screen.findByText(
				/This server has no signed agent 0\.2\.0 for x86_64\./,
			),
		).toBeInTheDocument();
	});

	it("shows neither button nor reason for an up-to-date agent", async () => {
		await openHost(detail({ version: "0.2.0" }), {
			version: "0.2.0",
			state: "queued",
		});
		await screen.findByText("Queued");
		expect(screen.queryByRole("button", { name: "Update agent" })).toBeNull();
		expect(screen.queryByText(/install\.sh/)).toBeNull();
	});
});
