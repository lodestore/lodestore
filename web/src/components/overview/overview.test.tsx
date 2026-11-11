import {
	act,
	cleanup,
	fireEvent,
	render,
	screen,
	waitFor,
	within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { delay, HttpResponse, http } from "msw";
import { beforeEach, describe, expect, it } from "vitest";
import type { FleetHost } from "@/api/gen/model";
import { StatusBadge } from "@/components/status-badge";
import { copy } from "@/copy";
import {
	alertCount,
	channelCount,
	packCount,
	problemCount,
} from "@/lib/plural";
import { renderApp, useSignedIn } from "@/test/app";
import {
	alertFixture,
	fleetHost,
	fleetOf,
	fleetOverview,
	fleetStorage,
	hostAlert,
	useAlertCount,
	useAlerts,
	useFleet,
} from "@/test/fleet";
import { channel, mockChannels } from "@/test/notifications";
import { server } from "@/test/server";

const fleetKey = "/api/v1/fleet";
const verdict = /attention$|protected$/;

const findSummary = () =>
	screen.findByRole("heading", { level: 2, name: verdict });
const legend = () =>
	within(screen.getByRole("group", { name: copy.overview.legend.label }));
const legendButton = (label: string, value: number) =>
	legend().getByRole("button", { name: new RegExp(`^${label}\\s*${value}$`) });
const storageLine = () =>
	screen
		.getByText(/^(Stored|Storage used)/, { selector: "p > span" })
		.closest("p") as HTMLElement;
const hostRows = () =>
	Array.from(document.querySelectorAll<HTMLElement>("[data-host-row]"));
const hostNames = () =>
	hostRows().map((row) => within(row).getAllByRole("link")[0].textContent);

async function renderOverview(path = "/overview") {
	const app = renderApp(path);
	await findSummary();
	return app;
}

describe("overview summary", () => {
	beforeEach(() => useSignedIn());

	it("shows the attention verdict and the state legend", async () => {
		useFleet(fleetOverview({ healthy: 5, overdue: 2, failed: 1, offline: 1 }));
		await renderOverview();

		expect(
			screen.getByRole("heading", { level: 1, name: copy.overview.title }),
		).toHaveAttribute("data-page-title");
		expect(screen.queryByText(/^Updated/)).toBeNull();
		const heading = await findSummary();
		expect(heading).toHaveTextContent("4 of 9 hosts need attention");
		expect(heading.querySelector("svg")).toHaveClass("text-destructive");

		const buttons = legend().getAllByRole("button");
		const filters = buttons.filter((b) => b.hasAttribute("aria-pressed"));
		expect(filters.map((b) => b.textContent)).toEqual([
			"Failed1",
			"Overdue2",
			"Offline1",
			"Protected5",
		]);
		const help = buttons[buttons.length - 1];
		expect(help).toHaveAccessibleName(copy.overview.legend.help);
		expect(help).not.toHaveAttribute("aria-pressed");
		expect(legend().queryByText("Not protected")).toBeNull();
		expect(legendButton("Failed", 1)).toHaveAttribute("aria-pressed", "false");
	});

	it("uses the singular verdict and adds Not protected only when non-zero", async () => {
		useFleet(fleetOverview({ failed: 1, healthy: 3, not_protected: 2 }));
		await renderOverview();
		expect(await findSummary()).toHaveTextContent(
			"3 of 6 hosts need attention",
		);
		expect(legendButton("Not protected", 2)).toBeInTheDocument();
	});

	it("says a single host needs attention", async () => {
		useFleet(fleetOverview({ failed: 1, healthy: 3 }));
		await renderOverview();
		expect(await findSummary()).toHaveTextContent(
			"1 of 4 hosts needs attention",
		);
	});

	it("shows the all-protected moment", async () => {
		useFleet(
			fleetOf([
				fleetHost("healthy", {
					id: "h1",
					hostname: "a",
					last_success_at: Date.now() - 2 * 3_600_000,
				}),
				fleetHost("healthy", { id: "h2", hostname: "b" }),
				fleetHost("healthy", { id: "h3", hostname: "c" }),
			]),
		);
		await renderOverview();
		const heading = await findSummary();
		expect(heading).toHaveTextContent("All 3 hosts protected");
		expect(heading).toHaveClass("text-display");
		expect(screen.getByText(/^Newest backup/)).toHaveTextContent(
			"Newest backup 2 hours ago.",
		);
		expect(
			screen.queryByRole("group", { name: copy.overview.legend.label }),
		).toBeNull();
		expect(
			screen.queryByRole("button", { name: copy.overview.legend.help }),
		).toBeNull();
	});

	it("says your host is protected for a single host", async () => {
		useFleet(fleetOverview({ healthy: 1 }));
		await renderOverview();
		expect(await findSummary()).toHaveTextContent("Your host is protected");
	});

	it("filters hosts by state through the URL", async () => {
		const user = userEvent.setup();
		useFleet(fleetOf(mixed()));
		const { router } = await renderOverview();
		const overdue = legendButton("Overdue", 1);
		await user.click(overdue);
		expect(overdue).toHaveAttribute("aria-pressed", "true");
		expect(router.state.location.search).toEqual({ state: "overdue" });
		expect(hostNames()).toEqual(["d-overdue"]);

		await user.click(legendButton("Failed", 1));
		expect(legendButton("Overdue", 1)).toHaveAttribute("aria-pressed", "false");
		expect(hostNames()).toEqual(["e-failed"]);

		await user.click(legendButton("Failed", 1));
		expect(router.state.location.search).toEqual({});
		expect(hostRows()).toHaveLength(6);
	});

	it("reads the state filter from the URL and ignores a bad value", async () => {
		useFleet(fleetOf(mixed()));
		await renderOverview("/overview?state=failed");
		expect(hostNames()).toEqual(["e-failed"]);
		expect(legendButton("Failed", 1)).toHaveAttribute("aria-pressed", "true");
	});

	it("ignores an unknown state in the URL", async () => {
		useFleet(fleetOf(mixed()));
		await renderOverview("/overview?state=bogus");
		expect(hostRows()).toHaveLength(6);
	});

	it("shows the quiet storage line", async () => {
		useFleet(fleetOverview({ failed: 1, healthy: 2 }));
		await renderOverview();
		expect(storageLine()).toHaveTextContent(
			"Stored 10 GiB · dedup 3.4× · counted 2 hours ago",
		);
		expect(screen.queryByText("Storage")).toBeNull();
		expect(screen.queryByText("Deduplication")).toBeNull();
	});

	it("hides dedup when the logical size is zero", async () => {
		useFleet(
			fleetOverview(
				{ failed: 1, healthy: 2 },
				{ storage: { ...fleetStorage, logical_bytes: 0 } },
			),
		);
		await renderOverview();
		expect(storageLine()).toHaveTextContent(
			"Stored 10 GiB · counted 2 hours ago",
		);
	});

	it("shows one phrase before the first structure check", async () => {
		useFleet(
			fleetOverview(
				{ healthy: 1 },
				{ storage: { repositories: 1, counted: 0 } },
			),
		);
		await renderOverview();
		expect(
			screen.getByText(copy.overview.storage.notCounted),
		).toBeInTheDocument();
	});

	it("keeps storage and the notification hint in one block", async () => {
		useFleet(fleetOverview({ failed: 1, healthy: 2 }));
		await renderOverview();
		const facts = async () => {
			const link = await screen.findByRole("link", {
				name: "Set up notifications",
			});
			expect(link).toHaveAttribute("href", "/settings/notifications");
			expect(link).toHaveAttribute("data-variant", "outline");
			const row = storageLine().parentElement as HTMLElement;
			expect(row).toContainElement(link);
			expect(row).toHaveTextContent("Nobody is notified yet.");
			expect(row.querySelectorAll("svg[aria-hidden]")).toHaveLength(2);
		};
		await facts();
		cleanup();

		useFleet(fleetOverview({ healthy: 3 }));
		renderApp("/overview");
		await screen.findByText("All 3 hosts protected");
		await facts();
		cleanup();

		mockChannels([channel("pager")]);
		useFleet(fleetOverview({ failed: 1, healthy: 2 }));
		renderApp("/overview");
		await findSummary();
		expect(storageLine().parentElement?.querySelectorAll("p,a")).toHaveLength(
			1,
		);
		await waitFor(() =>
			expect(screen.queryByText(/Nobody is notified yet/)).toBeNull(),
		);
		expect(
			screen.queryByRole("link", { name: "Set up notifications" }),
		).toBeNull();
	});

	it("explains each state on the legend", async () => {
		const user = userEvent.setup();
		useFleet(fleetOverview({ healthy: 5, overdue: 2, failed: 1 }));
		await renderOverview();
		await user.hover(legendButton("Overdue", 2));
		expect(await screen.findByRole("tooltip")).toHaveTextContent(
			copy.overview.stateHelp.overdue,
		);
		expect(legendButton("Overdue", 2)).toHaveAttribute("aria-pressed", "false");
	});

	it("explains the states on tap and from the keyboard", async () => {
		const user = userEvent.setup();
		useFleet(fleetOverview({ healthy: 5, overdue: 2, failed: 1, offline: 1 }));
		const { router } = await renderOverview();
		const help = legend().getByRole("button", {
			name: copy.overview.legend.help,
		});

		await user.pointer({ keys: "[TouchA]", target: help });
		const dialog = await screen.findByRole("dialog", {
			name: copy.overview.legend.help,
		});
		const inside = within(dialog);
		expect(inside.getAllByRole("term").map((t) => t.textContent)).toEqual([
			"Failed",
			"Overdue",
			"Offline",
			"Not protected",
			"Protected",
		]);
		expect(inside.getAllByRole("definition").map((d) => d.textContent)).toEqual(
			[
				copy.overview.stateHelp.failed,
				copy.overview.stateHelp.overdue,
				copy.overview.stateHelp.offline,
				copy.overview.stateHelp.not_protected,
				copy.overview.stateHelp.healthy,
			],
		);
		expect(router.state.location.search).toEqual({});
		expect(legendButton("Overdue", 2)).toHaveAttribute("aria-pressed", "false");

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(help).toHaveFocus();
		await user.keyboard("{Enter}");
		expect(
			await screen.findByRole("dialog", { name: copy.overview.legend.help }),
		).toBeInTheDocument();
	});

	it("renders the protected badge quietly", () => {
		render(<StatusBadge status="protected" />);
		const badge = screen.getByText("Protected").closest("[data-status]");
		expect(badge).toHaveAttribute("data-status", "protected");
		expect(badge).toHaveClass("text-muted-foreground");
	});
});

describe("overview states", () => {
	beforeEach(() => useSignedIn());

	it("shows the empty fleet", async () => {
		renderApp("/overview");
		expect(
			await screen.findByText(copy.overview.empty.heading),
		).toBeInTheDocument();
		const add = screen.getByRole("link", { name: copy.overview.empty.action });
		expect(add).toHaveAttribute("href", "/hosts?add=true");
		expect(
			screen.queryByRole("heading", { level: 2, name: verdict }),
		).toBeNull();
	});

	it("shows repository alerts above the empty fleet", async () => {
		useAlerts([
			alertFixture("u1", {
				type: "repository_unavailable",
				host: null,
				policy: null,
				repository: { id: "repo-2", name: "offsite" },
				detail: "The server does not open it: config hash mismatch.",
				facts: { error: "config hash mismatch" },
			}),
		]);
		renderApp("/overview");
		expect(
			await screen.findByRole("link", { name: "offsite" }),
		).toHaveAttribute("href", "/settings/repositories/repo-2#checks");
		expect(rows(STRIP).map(titleOf)).toContain(
			"Repository offsite is unavailable",
		);
		expect(screen.getByText(copy.overview.empty.heading)).toBeInTheDocument();
	});

	it("shows the loading summary", async () => {
		server.use(
			http.get("/api/v1/fleet", async () => {
				await delay("infinite");
				return HttpResponse.json({});
			}),
		);
		renderApp("/overview");
		expect(
			await screen.findByRole("heading", { level: 1, name: "Overview" }),
		).toBeInTheDocument();
		expect(document.querySelectorAll("[data-skeleton-row]")).toHaveLength(5);
		expect(document.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(
			3 + 5 * 9,
		);
	});

	it("shows the load error and stale data", async () => {
		const user = userEvent.setup();
		server.use(
			http.get("/api/v1/fleet", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		const { queryClient } = renderApp("/overview");
		expect(
			await screen.findByText(copy.overview.loadFailed),
		).toBeInTheDocument();

		useFleet(fleetOverview({ healthy: 3 }));
		await user.click(screen.getByRole("button", { name: copy.overview.retry }));
		await findSummary();
		expect(screen.queryByText(copy.overview.loadFailed)).toBeNull();

		server.use(
			http.get("/api/v1/fleet", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		await act(() => queryClient.refetchQueries());
		expect(
			await screen.findByText(copy.overview.staleData[0], { exact: false }),
		).toBeInTheDocument();
		expect(
			screen.getByRole("heading", { level: 2, name: verdict }),
		).toBeInTheDocument();
		expect(screen.queryByText(copy.overview.loadFailed)).toBeNull();
	});

	it("pluralizes phase counters", () => {
		expect([1, 3].map(alertCount)).toEqual(["1 alert", "3 alerts"]);
		expect([1, 3].map(problemCount)).toEqual(["1 problem", "3 problems"]);
		expect([1, 3].map(channelCount)).toEqual(["1 channel", "3 channels"]);
		expect([1, 3].map(packCount)).toEqual(["1 pack", "3 packs"]);
	});

	it.each([
		["overdue", "Overdue"],
		["silenced", "Silenced"],
		["passed", "Passed"],
		["not_protected", "Not protected"],
		["never_run", "Never run"],
	] as const)("renders the %s badge with an icon and word", (status, word) => {
		render(<StatusBadge status={status} />);
		const badge = screen.getByText(word).closest("[data-status]");
		expect(badge).toHaveAttribute("data-status", status);
		expect(badge?.querySelector("svg")).not.toBeNull();
	});
});

const DAY = 86_400_000;
const alertsKey = "/api/v1/alerts";
const HOST_ALERTS = "Alerts for this host";
const STRIP = "Repository and server alerts";

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};

const rows = (name: string) =>
	within(screen.getByRole("list", { name })).getAllByRole("listitem");
const titleOf = (row: HTMLElement) =>
	row.querySelector("[data-slot=item-title]")?.textContent;
const detailOf = (row: HTMLElement) =>
	row.querySelector("[data-slot=item-description]")?.textContent;

async function renderAlerts(
	alerts: ReturnType<typeof alertFixture>[],
	total?: number,
) {
	useFleet(fleetOverview({ healthy: 3 }));
	useAlerts(alerts, total);
	const app = renderApp("/overview");
	await findSummary();
	await waitFor(() => expect(app.queryClient.isFetching()).toBe(0));
	return app;
}

async function openHostAlerts(
	alerts: ReturnType<typeof alertFixture>[],
	total?: number,
	health: Partial<FleetHost> = {},
) {
	const now = Date.now();
	server.use(
		http.get("/api/v1/hosts/host-1", () =>
			HttpResponse.json({
				id: "host-1",
				hostname: "web-1",
				os: "linux",
				arch: "arm64",
				distro: null,
				version: "0.1.0",
				status: "online",
				labels: [],
				repository_id: "repo-1",
				enrolled_at: now - 30 * DAY,
				last_seen_at: now - 20_000,
				cert_not_after: now + 60 * DAY,
				cert_issued_at: now - 30 * DAY,
				next_renewal_at: now + 30 * DAY,
				repository_name: "main",
				kernel: "6.8.0-45-generic",
				uptime_s: 3_600,
				mounts: [],
			}),
		),
		http.get("/api/v1/hosts/host-1/policies", () => HttpResponse.json([])),
		http.get("/api/v1/hosts/host-1/snapshots", () =>
			HttpResponse.json({ items: [], page: 1, page_size: 50, total: 0 }),
		),
		http.get("/api/v1/jobs", () =>
			HttpResponse.json({ items: [], page: 1, page_size: 50, total: 0 }),
		),
		http.get("/api/v1/fleet/hosts/host-1", () =>
			HttpResponse.json(
				fleetHost("healthy", { id: "host-1", hostname: "web-1", ...health }),
			),
		),
	);
	useAlerts(alerts, total);
	const app = renderApp("/hosts/host-1");
	await screen.findByRole(
		"heading",
		{ level: 1, name: "web-1" },
		{ timeout: 5000 },
	);
	return app;
}

describe("active alerts", () => {
	beforeEach(() => useSignedIn());

	it("shows repository and server alerts in a strip", async () => {
		await renderAlerts([
			alertFixture("a1", {
				type: "check_failed",
				host: null,
				policy: null,
				repository: { id: "repo-1", name: "main" },
				check_level: "structure",
				facts: { findings: 3 },
			}),
			alertFixture("a2", {
				type: "certificate_expiring",
				severity: "warning",
				cert: "grpc",
				host: null,
				policy: null,
				detail: "The built-in certificate is renewed when the server restarts.",
				facts: { not_after: Date.now() + 8.5 * DAY },
			}),
			alertFixture("a3"),
		]);
		const strip = rows(STRIP);
		expect(strip.map(titleOf)).toEqual([
			"Structure check failed on main",
			"Server gRPC certificate expires in 9 days",
		]);
		expect(detailOf(strip[0])).toBe(
			"3 problems found. Open the report in Integrity checks.",
		);
		expect(screen.getByRole("link", { name: "main" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#checks",
		);
		expect(screen.queryByText("Backup failed on web-1")).toBeNull();
		expect(screen.queryByRole("heading", { name: "Active alerts" })).toBeNull();
		expect(
			screen.queryByRole("button", { name: /^Silence alerts/ }),
		).toBeNull();
	});

	it("leaves out the strip without repository or server alerts", async () => {
		await renderAlerts([alertFixture("a1")]);
		expect(screen.queryByRole("list", { name: STRIP })).toBeNull();
		expect(screen.queryByText("No active alerts.")).toBeNull();
	});

	it("notes when only part of the alerts is loaded", async () => {
		const hostIds: string[] = [];
		const alerts = Array.from({ length: 500 }, (_, i) =>
			alertFixture(`a${i}`, { host: { id: `h${i}`, hostname: `web-${i}` } }),
		);
		useFleet(fleetOverview({ healthy: 3 }));
		server.use(
			http.get(alertsKey, ({ request }) => {
				const id = new URL(request.url).searchParams.get("host_id");
				if (id) hostIds.push(id);
				return HttpResponse.json({ alerts, total: 612 });
			}),
		);
		renderApp("/overview");
		expect(
			await screen.findByText(
				"Showing 500 of 612 active alerts. Use the API to list all alerts.",
			),
		).toBeInTheDocument();
		expect(hostIds).toEqual([]);
	});

	it("says nothing about the limit when every alert is loaded", async () => {
		await renderAlerts([alertFixture("a1")], 1);
		expect(screen.queryByText(/active alerts. Use the API/)).toBeNull();
	});

	it("asks to add a channel while none exist", async () => {
		await renderAlerts([]);
		expect(
			await screen.findByRole("link", { name: "Set up notifications" }),
		).toHaveAttribute("href", "/settings/notifications");
	});

	it("orders and expands alerts", async () => {
		const user = userEvent.setup();
		const now = Date.now();
		await openHostAlerts([
			alertFixture("a1", {
				type: "no_recent_backup",
				severity: "warning",
				started_at: now - 5 * DAY,
				facts: { last_success_at: now - 3 * DAY, limit_secs: 2 * 86_400 },
			}),
			alertFixture("a2", { policy: "p2", started_at: now - 4 * DAY }),
			alertFixture("a3", { policy: "p3", started_at: now - 1 * DAY }),
			alertFixture("a4", {
				type: "agent_offline",
				severity: "warning",
				started_at: now - 1 * DAY,
				facts: { last_seen_at: now - 2 * DAY },
			}),
			alertFixture("a5", { policy: "p5", silenced: true }),
			alertFixture("a6", {
				type: "agent_offline",
				severity: "warning",
				silenced: true,
				facts: {},
			}),
			alertFixture("a7", {
				type: "agent_offline",
				severity: "warning",
				started_at: now - 9 * DAY,
				facts: {},
			}),
		]);

		await screen.findByRole("list", { name: HOST_ALERTS });
		expect(rows(HOST_ALERTS).map(detailOf)).toEqual([
			"Policy p3: disk full.",
			"Policy p2: disk full.",
			"Last contact 2 days ago.",
			"Last success 3 days ago. Expected at least every 2 days.",
			"The agent has not connected yet.",
		]);
		expect(screen.getByText("7")).toBeInTheDocument();

		const toggle = screen.getByRole("button", { name: "Show all 7 alerts" });
		expect(toggle).toHaveAttribute("aria-expanded", "false");
		await user.click(toggle);
		expect(toggle).toHaveAttribute("aria-expanded", "true");
		expect(toggle).toHaveTextContent("Show fewer alerts");
		expect(rows(HOST_ALERTS).map(detailOf).slice(5)).toEqual([
			"Policy p5: disk full.",
			"The agent has not connected yet.",
		]);
	});

	it("caps the alert list at 50", async () => {
		const user = userEvent.setup();
		const alerts = Array.from({ length: 50 }, (_, i) =>
			alertFixture(`a${i}`, { started_at: Date.now() - i * 60_000 }),
		);
		await openHostAlerts(alerts, 60);
		await user.click(
			await screen.findByRole("button", { name: "Show all 60 alerts" }),
		);
		expect(rows(HOST_ALERTS)).toHaveLength(50);
		expect(
			screen.getByText("and 10 more. Use the API to list all alerts."),
		).toBeInTheDocument();
	});

	it("builds alert titles and details", async () => {
		const now = Date.now();
		await openHostAlerts([
			alertFixture("a1", { facts: { error: "disk full." } }),
			alertFixture("a2", {
				type: "no_recent_backup",
				severity: "warning",
				facts: { last_success_at: now - 3 * DAY, limit_secs: 2 * 86_400 },
			}),
			alertFixture("a3", {
				type: "agent_offline",
				severity: "warning",
				facts: { last_seen_at: now - 2 * 3_600_000 },
			}),
			alertFixture("a5", {
				type: "certificate_expiring",
				severity: "warning",
				cert: "agent",
				detail: "Renews automatically while the agent is online.",
				facts: { not_after: now + 8.5 * DAY },
			}),
			alertFixture("a6", {
				type: "agent_update_failed",
				facts: {
					error: "Signature check failed: key not trusted on this host.",
				},
			}),
		]);
		await screen.findByRole("list", { name: HOST_ALERTS });

		const text = new Map(
			rows(HOST_ALERTS).map((row) => [titleOf(row), detailOf(row)]),
		);
		expect(text.get("Backup failed on web-1")).toBe("Policy daily: disk full.");
		expect(text.get("No recent backup on web-1")).toBe(
			"Last success 3 days ago. Expected at least every 2 days.",
		);
		expect(text.get("web-1 is offline")).toBe("Last contact 2 hours ago.");
		expect(text.get("Agent certificate on web-1 expires in 9 days")).toBe(
			"Renews automatically while the agent is online.",
		);
		expect(text.get("Agent update failed on web-1")).toBe(
			"Signature check failed: key not trusted on this host.",
		);
		expect(
			within(screen.getByRole("list", { name: HOST_ALERTS })).getAllByRole(
				"link",
				{ name: "web-1" },
			)[0],
		).toHaveAttribute("href", "/hosts/host-1");
	});

	it("links quarantine and unavailable alerts to the repository page", async () => {
		await renderAlerts([
			alertFixture("q1", {
				type: "snapshot_quarantined",
				host: null,
				policy: null,
				repository: { id: "repo-1", name: "main" },
				detail: "Review them under Free up space.",
				facts: { findings: 2, error: "snapshots/ab: decryption failed" },
			}),
			alertFixture("u1", {
				type: "repository_unavailable",
				host: null,
				policy: null,
				repository: { id: "repo-2", name: "offsite" },
				detail: "The server does not open it: config hash mismatch.",
				facts: { error: "config hash mismatch" },
			}),
		]);
		const titles = rows(STRIP).map(titleOf);
		expect(titles).toContain(
			"2 snapshots in main cannot be read; cleanup is stopped",
		);
		expect(titles).toContain("Repository offsite is unavailable");
		expect(screen.getByRole("link", { name: "main" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#free-up-space",
		);
		expect(screen.getByRole("link", { name: "offsite" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-2#checks",
		);
	});

	it("shows agent security alerts on the host", async () => {
		await openHostAlerts([
			alertFixture("s1", {
				type: "agent_session_conflict",
				severity: "warning",
				policy: null,
				detail:
					"Two live sessions of this agent from 203.0.113.9 and 198.51.100.4.",
				facts: { ip: "203.0.113.9", previous_ip: "198.51.100.4" },
			}),
			alertFixture("s2", {
				type: "agent_certificate_rejected",
				policy: null,
				detail: "The server refused the agent certificate with serial abc.",
				facts: { serial: "abc", ip: "203.0.113.9" },
			}),
		]);
		await screen.findByRole("list", { name: HOST_ALERTS });
		const titles = rows(HOST_ALERTS).map(titleOf);
		expect(titles).toContain("Two live agent sessions for web-1");
		expect(titles).toContain("Agent certificate refused for web-1");
		expect(
			within(screen.getByRole("list", { name: HOST_ALERTS })).getAllByRole(
				"link",
				{ name: "web-1" },
			)[0],
		).toHaveAttribute("href", "/hosts/host-1");
	});

	it("silences and resumes a host", async () => {
		const user = userEvent.setup();
		let body: unknown;
		server.use(
			http.put("/api/v1/hosts/host-1/silence", async ({ request }) => {
				body = await request.json();
				return HttpResponse.json({
					host_id: "host-1",
					created_at: Date.now(),
					created_by: "admin",
					until: Date.now() + DAY,
				});
			}),
			http.delete(
				"/api/v1/hosts/host-1/silence",
				() => new HttpResponse(null, { status: 204 }),
			),
		);
		const failed = (silenced: boolean) =>
			fleetHost("failed", {
				id: "host-1",
				hostname: "web-1",
				silenced,
				silenced_until: silenced ? Date.now() + DAY : undefined,
			});
		useFleet(fleetOf([failed(false)]));
		useAlerts([alertFixture("a1")]);
		const { queryClient } = renderApp("/overview");
		await findSummary();

		await user.click(
			await screen.findByRole("button", { name: "Actions for web-1" }),
		);
		await user.click(screen.getByRole("menuitem", { name: "Silence alerts" }));
		fireEvent.click(await screen.findByRole("menuitem", { name: "For 1 day" }));
		expect(
			await screen.findByText("Alerts for web-1 are silenced for 1 day."),
		).toBeInTheDocument();
		expect(body).toEqual({ duration: "day" });

		useFleet(fleetOf([failed(true)]));
		useAlerts([
			alertFixture("a1", { silenced: true, silenced_until: Date.now() + DAY }),
		]);
		await act(() => queryClient.refetchQueries({ queryKey: [fleetKey] }));
		await waitFor(() =>
			expect(hostRows()[0]).toHaveTextContent(/Silenced · until /),
		);
		await user.click(screen.getByRole("button", { name: "Actions for web-1" }));
		await user.click(screen.getByRole("menuitem", { name: "Silence alerts" }));
		fireEvent.click(
			await screen.findByRole("menuitem", { name: "Resume alerts" }),
		);
		expect(
			await screen.findByText("Alerts for web-1 resumed."),
		).toBeInTheDocument();
	});

	it("shows the silenced badge and reports silence errors", async () => {
		const user = userEvent.setup();
		server.use(
			http.put("/api/v1/hosts/host-1/silence", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		await openHostAlerts(
			[
				alertFixture("a1", {
					silenced: true,
					silenced_until: Date.now() + DAY,
				}),
			],
			undefined,
			{ silenced: true, silenced_until: Date.now() + DAY },
		);
		expect(await screen.findByText(/^Silenced · until /)).toBeInTheDocument();
		await user.click(
			await screen.findByRole("button", { name: "Silence alerts for web-1" }),
		);
		const [resume, hour] = await screen.findAllByRole("menuitem");
		expect(resume).toHaveTextContent("Resume alerts");
		await user.click(hour);
		expect(
			await screen.findByText(
				"Couldn't silence alerts for web-1. Boom. Try again.",
			),
		).toBeInTheDocument();
	});

	it("keeps alert rows flat inside cards", async () => {
		await renderAlerts([
			alertFixture("a1", {
				type: "check_failed",
				host: null,
				policy: null,
				repository: { id: "repo-1", name: "main" },
				check_level: "structure",
				facts: { findings: 1 },
			}),
		]);
		const nested = "[data-slot=card] [data-slot=item][data-variant=outline]";
		expect(rows(STRIP)).toHaveLength(1);
		expect(document.querySelector(nested)).toBeNull();
		cleanup();

		await openHostAlerts([alertFixture("a1")]);
		await screen.findByRole("list", { name: HOST_ALERTS });
		expect(document.querySelector("[data-slot=card]")).not.toBeNull();
		expect(document.querySelector(nested)).toBeNull();
		expect(
			within(screen.getByRole("list", { name: HOST_ALERTS })).queryByRole(
				"button",
				{ name: /^Silence alerts/ },
			),
		).toBeNull();
	});

	it("announces new alerts once", async () => {
		const first = alertFixture("a1");
		const { queryClient } = await renderAlerts([first]);
		expect(screen.queryByText(/new alert/)).toBeNull();

		const poll = () =>
			act(() => queryClient.refetchQueries({ queryKey: [alertsKey] }));
		const second = alertFixture("a2", {
			host: { id: "host-2", hostname: "web-2" },
		});
		useAlerts([first, second]);
		await poll();
		expect(await screen.findAllByText("1 new alert.")).toHaveLength(1);

		useAlerts([first, second]);
		await poll();
		expect(screen.getAllByText("1 new alert.")).toHaveLength(1);

		useAlerts([first]);
		await poll();
		await waitFor(() => expect(screen.queryByText(/new alert/)).toBeNull());
	});

	it("polls every 10 seconds and keeps the summary out of live regions", async () => {
		const { queryClient } = await renderAlerts([alertFixture("a1")]);
		const query = queryClient
			.getQueryCache()
			.find({ queryKey: [alertsKey, { state: "active", limit: 500 }] });
		expect(query?.observers[0].options.refetchInterval).toBe(10_000);
		const summary = screen.getByRole("heading", { level: 2, name: verdict });
		expect(summary.closest("[aria-live],[role=status]")).toBeNull();
		expect(summary.querySelector("[aria-live],[role=status]")).toBeNull();
		expect(document.querySelectorAll("[role=status]")).toHaveLength(1);
		expect(
			screen.getByRole("table").closest("[aria-live],[role=status]"),
		).toBeNull();
	});
});

const mixed = () => [
	fleetHost("healthy", { id: "h1", hostname: "a-healthy" }),
	fleetHost("not_protected", { id: "h2", hostname: "b-bare" }),
	fleetHost("offline", { id: "h3", hostname: "c-offline" }),
	fleetHost("overdue", {
		id: "h4",
		hostname: "d-overdue",
		last_success_at: Date.now() - 3 * DAY,
		worst: {
			policy: "daily",
			problem: "overdue",
			age_secs: 3 * 86_400,
			limit_secs: 2 * 86_400,
		},
	}),
	fleetHost("failed", {
		id: "h5",
		hostname: "e-failed",
		worst: {
			policy: "daily",
			problem: "failed",
			age_secs: 60,
			error: "disk full.",
		},
	}),
	fleetHost("healthy", { id: "h6", hostname: "0-healthy" }),
];

async function renderTable(hosts = mixed(), path = "/overview") {
	useFleet(fleetOf(hosts));
	const app = renderApp(path);
	await findSummary();
	return app;
}

describe("fleet table", () => {
	beforeEach(() => useSignedIn());

	it("renders hosts problems first", async () => {
		await renderTable();
		expect(hostNames()).toEqual([
			"e-failed",
			"d-overdue",
			"c-offline",
			"b-bare",
			"0-healthy",
			"a-healthy",
		]);
		expect(
			screen.getByText("daily · no success for 3 days, limit 2 days"),
		).toBeInTheDocument();
		expect(screen.getByText("daily · disk full")).toBeInTheDocument();
		const header = screen.getByRole("columnheader", { name: /^Host/ });
		expect(header).not.toHaveAttribute("aria-sort");
	});

	it("clears an empty filter", async () => {
		const user = userEvent.setup();
		await renderTable(
			[
				fleetHost("healthy", { id: "h1", hostname: "web-1" }),
				fleetHost("overdue", { id: "h2", hostname: "web-2" }),
			],
			"/overview?state=failed",
		);
		expect(legendButton("Failed", 0)).toHaveAttribute("aria-pressed", "true");
		expect(screen.getByText("No hosts match this filter.")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Clear filter" }));
		expect(hostNames()).toEqual(["web-2", "web-1"]);
		expect(legend().queryByRole("button", { name: /^Failed/ })).toBeNull();
	});

	it("has no host search on the overview", async () => {
		await renderTable();
		expect(screen.queryByRole("searchbox")).toBeNull();
		expect(
			Array.from(document.querySelectorAll("th[scope=colgroup]")).map(
				(th) => th.textContent,
			),
		).toEqual(["Needs attention · 4", "Protected · 2"]);
	});

	it("shows whole labels with a count of the rest", async () => {
		await renderTable([
			fleetHost("healthy", {
				id: "h1",
				hostname: "web-1",
				labels: [{ key: "role", value: "web" }],
			}),
			fleetHost("healthy", {
				id: "h2",
				hostname: "pg-1",
				labels: [
					{ key: "role", value: "db" },
					{ key: "env", value: "prod" },
					{ key: "dc", value: "eu" },
				],
			}),
		]);
		expect(screen.getByText("role=web")).toBeInTheDocument();
		expect(screen.getByText("role=db +2")).toBeInTheDocument();
		expect(screen.getByText("role=db env=prod +1")).toBeInTheDocument();
	});

	it("shortens the silenced badge on narrow tables", async () => {
		await renderTable([
			fleetHost("failed", {
				id: "h1",
				hostname: "db-1",
				silenced: true,
				silenced_until: Date.now() + 3_600_000,
			}),
		]);
		const [row] = hostRows();
		expect(within(row).getByText("Silenced")).toBeInTheDocument();
		expect(within(row).getByText(/^Silenced · until /)).toBeInTheDocument();
	});

	it("sorts and paginates", async () => {
		const user = userEvent.setup();
		const hosts = Array.from({ length: 120 }, (_, i) =>
			fleetHost("healthy", {
				id: `h${i}`,
				hostname: `host-${String(i).padStart(3, "0")}`,
				last_success_at: Date.now() - i * 3_600_000,
			}),
		);
		await renderTable(hosts);
		expect(hostRows()).toHaveLength(50);
		expect(screen.getByText("Showing 1-50 of 120")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Next" }));
		expect(screen.getByText("Showing 51-100 of 120")).toBeInTheDocument();
		expect(hostNames()[0]).toBe("host-050");

		const host = screen.getByRole("columnheader", { name: /^Host/ });
		await user.click(within(host).getByRole("button"));
		expect(host).toHaveAttribute("aria-sort", "ascending");
		expect(screen.getByText("Showing 1-50 of 120")).toBeInTheDocument();
		await user.click(within(host).getByRole("button"));
		expect(host).toHaveAttribute("aria-sort", "descending");
		expect(hostNames()[0]).toBe("host-119");

		const last = screen.getByRole("columnheader", {
			name: /^Last success/,
		});
		await user.click(within(last).getByRole("button"));
		expect(last).toHaveAttribute("aria-sort", "descending");
		expect(host).not.toHaveAttribute("aria-sort");
		expect(hostNames()[0]).toBe("host-000");
	});

	it("groups hosts into needs attention and protected", async () => {
		const user = userEvent.setup();
		await renderTable([
			fleetHost("healthy", { id: "h1", hostname: "z-ok" }),
			fleetHost("healthy", { id: "h2", hostname: "y-ok" }),
			fleetHost("not_protected", { id: "h3", hostname: "e-bare" }),
			fleetHost("offline", { id: "h4", hostname: "d-off" }),
			fleetHost("overdue", { id: "h5", hostname: "c-late" }),
			fleetHost("failed", { id: "h6", hostname: "b-bad" }),
			fleetHost("failed", { id: "h7", hostname: "a-quiet", silenced: true }),
		]);
		const headers = () =>
			Array.from(document.querySelectorAll("th[scope=colgroup]")).map(
				(th) => th.textContent,
			);
		expect(headers()).toEqual(["Needs attention · 5", "Protected · 2"]);
		expect(hostNames()).toEqual([
			"b-bad",
			"c-late",
			"d-off",
			"e-bare",
			"a-quiet",
			"y-ok",
			"z-ok",
		]);

		const host = screen.getByRole("columnheader", { name: /^Host/ });
		await user.click(within(host).getByRole("button"));
		await user.click(within(host).getByRole("button"));
		expect(hostNames()).toEqual([
			"e-bare",
			"d-off",
			"c-late",
			"b-bad",
			"a-quiet",
			"z-ok",
			"y-ok",
		]);
		expect(headers()).toEqual(["Needs attention · 5", "Protected · 2"]);
	});

	it("folds host alerts into one row", async () => {
		const host = fleetHost("failed", {
			id: "h1",
			hostname: "db-1",
			worst: {
				policy: "daily",
				problem: "failed",
				age_secs: 60,
				error: "disk full",
			},
		});
		useAlerts([
			hostAlert(host, "backup_failed", {
				started_at: Date.now() - 2 * 3_600_000,
			}),
			hostAlert(host, "agent_offline", {
				started_at: Date.now() - 30 * 60_000,
			}),
			hostAlert(host, "backup_failed", {
				id: "other",
				started_at: Date.now() - 60_000,
			}),
		]);
		await renderTable([host]);
		const [row] = hostRows();
		await waitFor(() => expect(row).toHaveTextContent("Since 2 hours ago"));
		expect(row).toHaveTextContent("Failed");
		expect(row).toHaveTextContent("daily · disk full · Agent offline");
		expect(hostRows()).toHaveLength(1);
	});

	it("keeps the reason to one line with the full text in a tooltip", async () => {
		const user = userEvent.setup();
		const host = fleetHost("failed", {
			id: "h1",
			hostname: "db-1",
			worst: {
				policy: "daily",
				problem: "failed",
				age_secs: 60,
				error:
					"disk full\nwriting /var/lib/postgresql/data/base/16384/2619\nretry",
			},
		});
		await renderTable([host]);
		const reason = screen.getByText(/^daily · disk full/);
		expect(reason).toHaveClass("truncate");
		expect(reason.tagName).toBe("SPAN");
		await user.hover(reason);
		const tooltip = await screen.findByRole("tooltip");
		expect(tooltip).toHaveTextContent(
			"/var/lib/postgresql/data/base/16384/2619",
		);
		expect(tooltip.querySelector("wbr")).not.toBeNull();
	});

	it("explains hosts without a failed pair", async () => {
		await renderTable([
			fleetHost("offline", {
				id: "h1",
				hostname: "off-1",
				agent_last_seen_at: Date.now() - 3 * 3_600_000,
			}),
			fleetHost("offline", { id: "h2", hostname: "off-2" }),
			fleetHost("not_protected", {
				id: "h3",
				hostname: "bare-1",
				has_policy: false,
			}),
			fleetHost("not_protected", {
				id: "h4",
				hostname: "bare-2",
				has_policy: true,
			}),
		]);
		expect(hostRows()[0]).not.toHaveTextContent("Since");
		for (const reason of [
			"Agent last seen 3 hours ago",
			"Agent has not connected yet",
			"No backup policy applies to this host",
			"No scheduled backup has succeeded yet",
		]) {
			expect(screen.getByText(reason)).toBeInTheDocument();
		}
	});

	it("names what each fix button opens", async () => {
		await renderTable([
			fleetHost("failed", {
				id: "h1",
				hostname: "db-1",
				worst: { policy: "daily", problem: "failed", age_secs: 1, error: "x" },
			}),
			fleetHost("overdue", {
				id: "h2",
				hostname: "db-2",
				worst: {
					policy: "weekly",
					problem: "overdue",
					age_secs: 9,
					limit_secs: 5,
				},
			}),
			fleetHost("offline", { id: "h3", hostname: "db-3" }),
			fleetHost("not_protected", {
				id: "h4",
				hostname: "db-4",
				has_policy: true,
			}),
			fleetHost("not_protected", {
				id: "h5",
				hostname: "db-5",
				has_policy: false,
			}),
			fleetHost("healthy", { id: "h6", hostname: "db-6" }),
		]);
		const fix = (name: string, label: string, href: string) => {
			const link = screen.getByRole("link", { name });
			expect(link).toHaveAttribute("href", href);
			expect(link).toHaveTextContent(label);
			expect(link).toHaveAttribute("data-variant", "outline");
		};
		fix("Open policy daily for db-1", "Open policy", "/policies/daily");
		fix("Open policy weekly for db-2", "Open policy", "/policies/weekly");
		fix("Check agent on db-3", "Check agent", "/hosts/h3");
		fix("Open host db-4", "Open host", "/hosts/h4");
		fix("Add policy for db-5", "Add policy", "/policies/new");
		expect(
			within(hostRows()[5]).queryByRole("link", { name: /^(Open|Check|Add) / }),
		).toBeNull();
	});

	it("keeps protected hosts quiet", async () => {
		const host = fleetHost("healthy", { id: "h1", hostname: "web-1" });
		const plain = fleetHost("healthy", { id: "h2", hostname: "web-2" });
		useAlerts([hostAlert(host, "certificate_expiring")]);
		await renderTable([host, plain]);
		const [row, quiet] = hostRows();
		await waitFor(() => expect(row).toHaveTextContent("Certificate expiring"));
		expect(row.querySelector("[data-status=protected]")).not.toBeNull();
		expect(
			within(row).queryByRole("link", { name: /^(Open|Check|Add) / }),
		).toBeNull();
		expect(quiet).not.toHaveTextContent("Since");
		expect(
			quiet.querySelectorAll("td")[1].querySelector("p,.truncate"),
		).toBeNull();
	});

	it("shows the filter chip next to the table", async () => {
		const user = userEvent.setup();
		const { router } = await renderTable(mixed(), "/overview?state=overdue");
		expect(hostNames()).toEqual(["d-overdue"]);
		const headers = Array.from(
			document.querySelectorAll("th[scope=colgroup]"),
		).map((th) => th.textContent);
		expect(headers).toEqual(["Needs attention · 1"]);
		await user.click(screen.getByRole("button", { name: /^Showing: Overdue/ }));
		expect(router.state.location.search).toEqual({});
		expect(hostRows()).toHaveLength(6);
		expect(screen.queryByRole("button", { name: /^Showing:/ })).toBeNull();
	});

	it("shows only the protected group for the healthy filter", async () => {
		await renderTable(mixed(), "/overview?state=healthy");
		expect(hostNames()).toEqual(["0-healthy", "a-healthy"]);
		expect(
			Array.from(document.querySelectorAll("th[scope=colgroup]")).map(
				(th) => th.textContent,
			),
		).toEqual(["Protected · 2"]);
	});

	it("moves between hosts with the arrow keys", async () => {
		const user = userEvent.setup();
		await renderTable();
		const name = (host: string) => screen.getByRole("link", { name: host });
		const focusedName = () => document.activeElement?.textContent;
		act(() => name("e-failed").focus());

		await user.keyboard("{ArrowDown}");
		expect(name("d-overdue")).toHaveFocus();
		await user.keyboard("{ArrowDown}");
		expect(name("c-offline")).toHaveFocus();
		await user.keyboard("{ArrowUp}");
		expect(name("d-overdue")).toHaveFocus();
		await user.keyboard("{End}");
		expect(name("a-healthy")).toHaveFocus();
		await user.keyboard("{ArrowDown}");
		expect(focusedName()).toBe("a-healthy");
		await user.keyboard("{ArrowUp}");
		expect(name("0-healthy")).toHaveFocus();
		await user.keyboard("{ArrowUp}");
		expect(name("b-bare")).toHaveFocus();
		await user.keyboard("{Home}");
		expect(name("e-failed")).toHaveFocus();
		await user.keyboard("{ArrowUp}");
		expect(focusedName()).toBe("e-failed");
	});

	it("opens the focused host with Enter", async () => {
		const user = userEvent.setup();
		const { router } = await renderTable();
		act(() => screen.getByRole("link", { name: "e-failed" }).focus());
		await user.keyboard("{ArrowDown}");
		await user.keyboard("{Enter}");
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/hosts/h4"),
		);
	});

	it("keeps one host row in the tab order", async () => {
		const user = userEvent.setup();
		await renderTable();
		const stops = (row: HTMLElement) =>
			Array.from(row.querySelectorAll<HTMLElement>("a,button,time"));
		const tabs = (row: HTMLElement) => stops(row).map((el) => el.tabIndex);
		const rows = () => hostRows();

		expect(tabs(rows()[0]).every((t) => t === 0)).toBe(true);
		for (const row of rows().slice(1)) {
			expect(tabs(row).every((t) => t === -1)).toBe(true);
		}

		act(() => screen.getByRole("link", { name: "e-failed" }).focus());
		await user.keyboard("{ArrowDown}");
		expect(tabs(rows()[1]).every((t) => t === 0)).toBe(true);
		expect(tabs(rows()[0]).every((t) => t === -1)).toBe(true);

		const seen: string[] = [];
		for (let i = 0; i < 12; i++) {
			await user.tab();
			const row = document.activeElement?.closest("[data-host-row]");
			if (!row) break;
			expect(row).toBe(rows()[1]);
			seen.push(
				document.activeElement?.getAttribute("aria-label") ??
					document.activeElement?.tagName ??
					"",
			);
		}
		expect(document.activeElement?.closest("[data-host-row]")).toBeNull();
		expect(seen).toContain("Open policy daily for d-overdue");
		expect(seen).toContain("Actions for d-overdue");
	});

	it("leaves the row menu its own keys", async () => {
		const user = userEvent.setup();
		await renderTable();
		const trigger = screen.getByRole("button", {
			name: "Actions for e-failed",
		});
		act(() => trigger.focus());
		await user.keyboard("{ArrowDown}");
		const menu = await screen.findByRole("menu");
		await waitFor(() =>
			expect(menu).toContainElement(document.activeElement as HTMLElement),
		);
		await user.keyboard("{ArrowDown}");
		expect(menu).toContainElement(document.activeElement as HTMLElement);
		expect(document.activeElement?.closest("[data-host-row]")).toBeNull();
		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
		expect(trigger).toHaveFocus();
	});

	it("moves on from the row menu button with the other keys", async () => {
		const user = userEvent.setup();
		await renderTable();
		act(() =>
			screen.getByRole("button", { name: "Actions for d-overdue" }).focus(),
		);
		await user.keyboard("{ArrowUp}");
		expect(screen.getByRole("link", { name: "e-failed" })).toHaveFocus();
		act(() =>
			screen.getByRole("button", { name: "Actions for e-failed" }).focus(),
		);
		await user.keyboard("{End}");
		expect(screen.getByRole("link", { name: "a-healthy" })).toHaveFocus();
	});

	it("offers row actions", async () => {
		const user = userEvent.setup();
		let run: unknown;
		server.use(
			http.post("/api/v1/hosts/h1/run", async ({ request }) => {
				run = await request.json();
				return HttpResponse.json({
					jobs: [{ id: "j1", host_id: "h1", hostname: "web-1", created: true }],
				});
			}),
			http.put("/api/v1/hosts/h2/silence", () => {
				return HttpResponse.json({
					host_id: "h2",
					created_at: Date.now(),
					created_by: "admin",
				});
			}),
		);
		await renderTable([
			fleetHost("healthy", { id: "h1", hostname: "web-1" }),
			fleetHost("not_protected", {
				id: "h2",
				hostname: "web-2",
				has_policy: false,
			}),
		]);

		await user.click(screen.getByRole("button", { name: "Actions for web-2" }));
		expect(
			await screen.findByRole("menuitem", { name: "View host" }),
		).toBeInTheDocument();
		expect(screen.queryByRole("menuitem", { name: "Run backup" })).toBeNull();
		await user.click(screen.getByRole("menuitem", { name: "Silence alerts" }));
		expect(await screen.findAllByRole("menuitem")).toHaveLength(6);
		fireEvent.click(
			await screen.findByRole("menuitem", { name: "For 1 hour" }),
		);
		expect(
			await screen.findByText("Alerts for web-2 are silenced for 1 hour."),
		).toBeInTheDocument();

		await user.click(screen.getByRole("button", { name: "Actions for web-1" }));
		await user.click(
			await screen.findByRole("menuitem", { name: "Run backup" }),
		);
		expect(
			await screen.findByText("Backup queued for web-1."),
		).toBeInTheDocument();
		expect(run).toEqual({ policies: null });
	});
});

describe("sidebar alert badge", () => {
	beforeEach(() => useSignedIn());

	it("shows the sidebar alert badge", async () => {
		useAlertCount(3, 5);
		await renderTable([fleetHost("healthy", { id: "h1", hostname: "web-1" })]);
		const badge = await screen.findByRole("img", { name: "3 active alerts" });
		expect(badge).toHaveTextContent("3");
		expect(screen.getByText("3 active alerts")).toHaveClass("sr-only");
	});

	it("caps the badge at 99+", async () => {
		useAlertCount(120);
		await renderTable([fleetHost("healthy", { id: "h1", hostname: "web-1" })]);
		const badge = await screen.findByRole("img", { name: "120 active alerts" });
		expect(badge).toHaveTextContent("99+");
	});

	it("hides the badge at zero and on error", async () => {
		await renderTable([fleetHost("healthy", { id: "h1", hostname: "web-1" })]);
		expect(screen.queryByRole("img", { name: /active alert/ })).toBeNull();
		server.use(
			http.get("/api/v1/alerts/count", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		const { queryClient } = renderApp("/overview");
		await act(() => queryClient.refetchQueries());
		expect(screen.queryByRole("img", { name: /active alert/ })).toBeNull();
	});
});
