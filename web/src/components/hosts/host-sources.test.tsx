import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { delay, HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	HostDetail,
	HostSources,
	Protection,
	SourceGroup,
	SourceItem,
} from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;
const ID = "pve10000-0000-4000-8000-000000000000";

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};
Element.prototype.hasPointerCapture ??= () => false;
Element.prototype.setPointerCapture ??= () => {};
Element.prototype.releasePointerCapture ??= () => {};
Element.prototype.scrollIntoView ??= () => {};

function makeHost(over: Partial<HostDetail> = {}): HostDetail {
	return {
		id: ID,
		hostname: "pve-01",
		os: "linux",
		arch: "amd64",
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
		uptime_s: 3_600,
		mounts: [],
		source_types: ["proxmox-vm", "proxmox-ct"],
		...over,
	};
}

function vm(vmid: number, name: string, over: Partial<SourceItem> = {}) {
	return {
		source_type: "proxmox-vm",
		label: `VM ${vmid} · ${name}`,
		name,
		vmid,
		pool: null,
		tags: [],
		protection: { state: "unprotected", policies: [] },
		last_snapshot_at: null,
		shared_with: [],
		...over,
	} as SourceItem;
}

function ct(vmid: number, name: string, over: Partial<SourceItem> = {}) {
	return vm(vmid, name, {
		source_type: "proxmox-ct",
		label: `CT ${vmid} · ${name}`,
		...over,
	});
}

function db(name: string, groupId: string, over: Partial<SourceItem> = {}) {
	return {
		source_type: "postgres",
		label: name,
		name,
		group_id: groupId,
		oid: 16384,
		pool: null,
		tags: [],
		protection: { state: "protected", policies: ["pg-nightly"] },
		last_snapshot_at: Date.now() - 2 * 3_600_000,
		shared_with: [],
		...over,
	} as SourceItem;
}

function group(id: string, over: Partial<SourceGroup> = {}): SourceGroup {
	return {
		id,
		source_type: "postgres",
		port: 5432,
		version: "16",
		unreachable: false,
		shared_with: [],
		...over,
	};
}

const bodyRows = () =>
	within(screen.getByRole("table")).getAllByRole("row").slice(1);

function makeSources(over: Partial<HostSources> = {}): HostSources {
	return {
		collected_at: Date.now() - 180_000,
		truncated: false,
		scope: null,
		groups: [],
		items: [],
		files: null,
		...over,
	};
}

function mockHost(host: HostDetail, sources: HostSources | (() => Response)) {
	useSignedIn();
	server.use(
		http.get(`/api/v1/hosts/${host.id}`, () => HttpResponse.json(host)),
		http.get(`/api/v1/hosts/${host.id}/sources`, () =>
			typeof sources === "function" ? sources() : HttpResponse.json(sources),
		),
	);
}

async function openSources(
	host: HostDetail,
	sources: HostSources | (() => Response),
	query = "tab=sources",
) {
	mockHost(host, sources);
	const app = renderApp(`/hosts/${host.id}?${query}`);
	await screen.findByRole("heading", { level: 1, name: host.hostname });
	return app;
}

const tabNames = () =>
	within(screen.getByRole("tablist"))
		.getAllByRole("tab")
		.map((tab) => tab.textContent);

describe("Host sources tab", () => {
	it("is absent on a host without discovered sources and falls back to Overview", async () => {
		const host = makeHost({ source_types: [] });
		mockHost(host, makeSources());
		renderApp(`/hosts/${host.id}?tab=sources`);
		await screen.findByRole("heading", { level: 1, name: "pve-01" });

		expect(tabNames()).toEqual(["Overview", "Snapshots", "Jobs"]);
		expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	});

	it("lists guests with their protection, between Overview and Snapshots", async () => {
		await openSources(
			makeHost(),
			makeSources({
				items: [
					vm(101, "web-01", {
						protection: { state: "agent_too_old", policies: [] },
					}),
					vm(103, "db-01", {
						protection: { state: "protected", policies: ["vms", "weekly"] },
						last_snapshot_at: Date.now() - 14 * 60_000,
					}),
					ct(200, "dns"),
				],
			}),
		);

		await screen.findByText("VM 101 · web-01");
		expect(tabNames()).toEqual(["Overview", "Sources", "Snapshots", "Jobs"]);
		const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);

		expect(within(rows[0]).getByText("Agent too old")).toBeInTheDocument();
		expect(within(rows[0]).getByText("Never")).toBeInTheDocument();

		expect(within(rows[1]).getByRole("link", { name: "vms" })).toHaveAttribute(
			"href",
			"/policies/vms",
		);
		expect(within(rows[1]).getByText("+1")).toBeInTheDocument();
		expect(within(rows[1]).getByText("14 minutes ago")).toBeInTheDocument();

		expect(within(rows[2]).getByText("CT 200 · dns")).toBeInTheDocument();
		expect(within(rows[2]).getByText("Not protected")).toBeInTheDocument();
		expect(screen.getByRole("table").textContent).not.toMatch(/source_ref/);
	});
});

const mixed = () =>
	makeSources({
		items: [
			vm(101, "web", { protection: { state: "protected", policies: ["vms"] } }),
			vm(102, "app"),
			ct(200, "dns", { protection: { state: "agent_too_old", policies: [] } }),
			vm(117, "ci"),
		],
	});

const tooOld: Protection = { state: "agent_too_old", policies: [] };

describe("Host sources toolbar", () => {
	it("filters to the sources without a policy and keeps the choice in the URL", async () => {
		const user = userEvent.setup();
		const { router } = await openSources(makeHost(), mixed());

		const toggle = await screen.findByRole("radiogroup", { name: "Show" });
		expect(within(toggle).getByRole("radio", { name: "All4" })).toBeChecked();
		expect(bodyRows()).toHaveLength(4);

		await user.click(
			within(toggle).getByRole("radio", { name: "Not protected2" }),
		);

		await waitFor(() => expect(bodyRows()).toHaveLength(2));
		expect(router.state.location.search).toMatchObject({ show: "unprotected" });
		expect(screen.queryByText("Agent too old")).toBeNull();
	});

	it("opens on the filter from the URL", async () => {
		await openSources(makeHost(), mixed(), "tab=sources&show=unprotected");
		await screen.findByText("VM 102 · app");
		expect(bodyRows()).toHaveLength(2);
	});

	it("shows everything for an unknown filter value", async () => {
		await openSources(makeHost(), mixed(), "tab=sources&show=bogus");
		await screen.findByText("VM 102 · app");
		expect(bodyRows()).toHaveLength(4);
	});

	it("drops the filter when nothing is left unprotected", async () => {
		await openSources(
			makeHost(),
			makeSources({
				items: [
					vm(101, "web", {
						protection: { state: "protected", policies: ["vms"] },
					}),
				],
			}),
			"tab=sources&show=unprotected",
		);
		await screen.findByText("VM 101 · web");
		expect(screen.queryByRole("radiogroup")).toBeNull();
		expect(screen.getByText("Found by the agent 3 minutes ago")).toBeVisible();
		expect(bodyRows()).toHaveLength(1);
	});

	it("names the cluster before the found-by text", async () => {
		await openSources(
			makeHost(),
			makeSources({
				scope: { kind: "cluster", name: "pve-prod" },
				items: [vm(101, "web")],
			}),
		);
		await screen.findByText(
			"Proxmox cluster pve-prod · found by the agent 3 minutes ago",
		);
	});

	it("names a node outside a cluster", async () => {
		await openSources(
			makeHost(),
			makeSources({
				scope: { kind: "node", name: "pve1" },
				items: [vm(101, "web")],
			}),
		);
		await screen.findByText(
			"Proxmox node pve1 · found by the agent 3 minutes ago",
		);
	});
});

describe("Host sources groups and pages", () => {
	const pg = ["postgres"];

	it("nests databases under their server and flags an unreachable one", async () => {
		await openSources(
			makeHost({ source_types: pg }),
			makeSources({
				groups: [
					group("postgres:5432"),
					group("postgres:5433", {
						version: "14",
						port: 5433,
						unreachable: true,
					}),
				],
				items: [
					db("app", "postgres:5432"),
					db("billing", "postgres:5432", { oid: 16385 }),
				],
			}),
		);

		await screen.findByText("PostgreSQL 16 · port 5432");
		expect(
			screen.getByRole("columnheader", { name: "Details" }),
		).toBeInTheDocument();
		const rows = bodyRows().map((row) => row.textContent);
		expect(rows[0]).toBe("PostgreSQL 16 · port 5432");
		expect(rows[1]).toContain("appOID 16384");
		expect(rows[2]).toContain("billingOID 16385");
		expect(rows[3]).toBe("PostgreSQL 14 · port 5433");
		expect(rows[4]).toBe(
			"The agent can't connect as the postgres user, so the databases are unknown.",
		);
		expect(rows).toHaveLength(5);
	});

	it("pages 25 sources at a time and repeats the server row and the unreachable one", async () => {
		const user = userEvent.setup();
		await openSources(
			makeHost({ source_types: pg }),
			makeSources({
				groups: [
					group("postgres:5432"),
					group("postgres:5433", { port: 5433, unreachable: true }),
				],
				items: Array.from({ length: 30 }, (_, i) =>
					db(`db${String(i).padStart(2, "0")}`, "postgres:5432"),
				),
			}),
		);

		await screen.findByText("Showing 1–25 of 30 sources");
		expect(screen.getByRole("button", { name: "Previous" })).toBeDisabled();
		expect(bodyRows()).toHaveLength(1 + 25 + 2);

		await user.click(screen.getByRole("button", { name: "Next" }));

		await screen.findByText("Showing 26–30 of 30 sources");
		expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
		const rows = bodyRows().map((row) => row.textContent);
		expect(rows[0]).toBe("PostgreSQL 16 · port 5432");
		expect(rows).toHaveLength(1 + 5 + 2);
		expect(rows[rows.length - 1]).toContain("databases are unknown");
	});

	it("has no pager for a short list", async () => {
		await openSources(makeHost(), mixed());
		await screen.findByText("VM 102 · app");
		expect(screen.queryByRole("button", { name: "Next" })).toBeNull();
	});

	it("shows the Files row for a host covered by a files policy", async () => {
		await openSources(
			makeHost(),
			makeSources({
				items: [vm(101, "web")],
				files: {
					policies: ["etc"],
					paths: ["/etc", "/home"],
					last_snapshot_at: Date.now() - 3_600_000,
				},
			}),
		);

		await screen.findByText("VM 101 · web");
		const rows = bodyRows();
		const last = rows[rows.length - 1];
		expect(within(last).getByText("Files")).toBeInTheDocument();
		expect(within(last).getByText("/etc, /home")).toBeInTheDocument();
		expect(within(last).getByRole("link", { name: "etc" })).toHaveAttribute(
			"href",
			"/policies/etc",
		);
	});
});

describe("Host sources notes", () => {
	it("lists the notes in order: truncated, agent too old, clones", async () => {
		await openSources(
			makeHost(),
			makeSources({
				truncated: true,
				items: [
					vm(101, "a", { protection: tooOld }),
					ct(200, "b", { protection: tooOld }),
					...[1, 2, 3, 4, 5].map((n) =>
						vm(300 + n, `clone${n}`, { shared_with: ["pve-02", "pve-03"] }),
					),
				],
			}),
		);

		const truncated = await screen.findByText(
			"The agent reported the first 7 sources only.",
		);
		const old = screen.getByText(
			"Agent too old: this agent version finds virtual machines and containers but can't back them up yet. Update the agent.",
		);
		const clone = screen.getByText(
			"VM 301 · clone1 is the same source as on pve-02, pve-03. Add --host to lode latest@ for this source.",
		);
		const follows = Node.DOCUMENT_POSITION_FOLLOWING;
		expect(truncated.compareDocumentPosition(old) & follows).toBeTruthy();
		expect(old.compareDocumentPosition(clone) & follows).toBeTruthy();
		expect(screen.getAllByText(/is the same source as on/)).toHaveLength(3);
		expect(
			screen.getByText("and 2 more sources share their key with other hosts."),
		).toBeInTheDocument();
	});

	it("words a cloned server as a cluster", async () => {
		await openSources(
			makeHost({ source_types: ["postgres"] }),
			makeSources({
				groups: [group("postgres:5432", { shared_with: ["db-03"] })],
				items: [db("app", "postgres:5432")],
			}),
		);
		await screen.findByText(
			"PostgreSQL 16 · port 5432 is the same cluster as on db-03. Add --host to lode latest@ for this source.",
		);
	});

	it("names only the types the agent cannot back up", async () => {
		await openSources(
			makeHost({ source_types: ["proxmox-vm"] }),
			makeSources({
				items: [
					vm(101, "a", { protection: tooOld }),
					vm(102, "b", { protection: tooOld }),
				],
			}),
		);
		await screen.findByText(
			"Agent too old: this agent version finds virtual machines but can't back them up yet. Update the agent.",
		);
		expect(screen.queryByText(/containers/)).toBeNull();
	});

	it("shows no notes when nothing calls for one", async () => {
		await openSources(makeHost(), mixed());
		await screen.findByText("VM 102 · app");
		expect(screen.queryByText(/is the same/)).toBeNull();
		expect(screen.queryByText(/reported the first/)).toBeNull();
	});
});

describe("Host sources states", () => {
	it("shows five skeleton rows and no toolbar until the first answer", async () => {
		const host = makeHost();
		useSignedIn();
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () => HttpResponse.json(host)),
			http.get(`/api/v1/hosts/${host.id}/sources`, async () => {
				await delay("infinite");
				return HttpResponse.json(makeSources());
			}),
		);
		renderApp(`/hosts/${host.id}?tab=sources`);

		await waitFor(() =>
			expect(document.querySelectorAll("tbody tr[aria-hidden]")).toHaveLength(
				5,
			),
		);
		expect(screen.queryByRole("radiogroup")).toBeNull();
		expect(screen.queryByText(/found by the agent/i)).toBeNull();
	});

	it("explains a failed load and retries on request", async () => {
		const user = userEvent.setup();
		const host = makeHost();
		let calls = 0;
		useSignedIn();
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () => HttpResponse.json(host)),
			http.get(`/api/v1/hosts/${host.id}/sources`, () => {
				calls += 1;
				return calls === 1
					? HttpResponse.json(
							{ code: "internal", message: "boom" },
							{ status: 500 },
						)
					: HttpResponse.json(mixed());
			}),
		);
		renderApp(`/hosts/${host.id}?tab=sources`);

		const alert = await screen.findByRole("alert");
		expect(alert).toHaveTextContent(
			"Couldn't load the sources. Check that the server is reachable, then retry.",
		);
		await user.click(
			within(alert).getByRole("button", { name: "Retry loading sources" }),
		);
		await screen.findByText("VM 102 · app");
	});

	it("says so when the agent found nothing", async () => {
		await openSources(makeHost(), makeSources());
		await screen.findByText("The agent found no sources on this host.");
		expect(
			screen.getByText("It checks again every few minutes."),
		).toBeInTheDocument();
		expect(screen.queryByRole("table")).toBeNull();
	});
});
