import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { HostDetail, HostSources, SourceItem } from "@/api/gen/model";
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
