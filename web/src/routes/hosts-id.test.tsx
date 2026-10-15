import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { HostDetail } from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};

function makeDetail(name: string, over: Partial<HostDetail> = {}): HostDetail {
	return {
		id: `${name.replace(/\W/g, "").padEnd(8, "0")}-0000-4000-8000-000000000000`,
		hostname: name,
		os: "linux",
		arch: "arm64",
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
		uptime_s: 3 * 86_400 + 4 * 3_600,
		mounts: [],
		...over,
	};
}

function mockDetail(detail: HostDetail) {
	server.use(
		http.get(`/api/v1/hosts/${detail.id}`, () => HttpResponse.json(detail)),
	);
}

async function openHost(detail: HostDetail, search = "") {
	mockDetail(detail);
	useSignedIn();
	const view = renderApp(`/hosts/${detail.id}${search}`);
	await screen.findByRole("heading", { level: 1, name: detail.hostname });
	return view;
}

describe("Host page", () => {
	it("shows the host name, status, last seen and breadcrumb", async () => {
		const host = makeDetail("web-1");
		await openHost(host);

		const heading = screen.getByRole("heading", { level: 1 });
		expect(heading).toHaveTextContent("web-1");
		expect(heading).toHaveAttribute("data-page-title");
		expect(screen.getAllByText("Online").length).toBeGreaterThan(0);
		expect(screen.getByText(/^Last seen /)).toBeInTheDocument();
		const crumbs = screen.getByRole("navigation", { name: "breadcrumb" });
		expect(within(crumbs).getByRole("link", { name: "Hosts" })).toHaveAttribute(
			"href",
			"/hosts",
		);
		expect(
			screen.getByRole("button", { name: "Edit labels" }),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Revoke host" }),
		).toBeInTheDocument();
	});

	it("links the repository to its settings page", async () => {
		await openHost(makeDetail("web-1"));
		expect(await screen.findByRole("link", { name: "main" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1",
		);
	});

	it("limits filesystems to 20 rows and expands on request", async () => {
		const host = makeDetail("web-1", {
			mounts: Array.from({ length: 60 }, (_, i) => ({
				path: `/mnt/fs${String(i).padStart(2, "0")}`,
				fs_type: "ext4",
				total_bytes: 100 * 1024 ** 3,
				free_bytes: (100 - i) * 1024 ** 3,
			})),
		});
		await openHost(host);

		const table = await screen.findByRole("table");
		expect(within(table).getAllByRole("row")).toHaveLength(21);
		expect(within(table).getAllByRole("row")[1]).toHaveTextContent("/mnt/fs59");
		expect(within(table).getAllByRole("row")[1]).toHaveTextContent("59%");

		await userEvent.click(
			screen.getByRole("button", { name: "Show all 60 filesystems" }),
		);
		expect(within(table).getAllByRole("row")).toHaveLength(61);
		expect(screen.queryByRole("button", { name: /Show all/ })).toBeNull();
	});

	it("shows a dash for missing facts and hides empty cards", async () => {
		await openHost(makeDetail("fresh-1", { kernel: null, uptime_s: null }));

		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		expect(screen.getAllByText("—").length).toBeGreaterThanOrEqual(3);
		expect(screen.queryByRole("heading", { name: "Storage" })).toBeNull();
		expect(screen.queryByRole("heading", { name: "Labels" })).toBeNull();
	});

	it("reports a load failure and retries", async () => {
		const host = makeDetail("web-1");
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () =>
				HttpResponse.json({ code: "x", message: "boom" }, { status: 500 }),
			),
		);
		useSignedIn();
		renderApp(`/hosts/${host.id}`);
		expect(
			await screen.findByText(/Couldn't load this host\./),
		).toBeInTheDocument();

		mockDetail(host);
		await userEvent.click(
			screen.getByRole("button", { name: "Retry loading" }),
		);
		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		await waitFor(() =>
			expect(screen.queryByText(/Couldn't load this host\./)).toBeNull(),
		);
	});

	it("shows all labels on the Labels card", async () => {
		await openHost(
			makeDetail("web-1", {
				labels: [
					{ key: "env", value: "prod" },
					{ key: "team", value: "core" },
				],
			}),
		);
		expect(
			await screen.findByRole("heading", { name: "Labels" }),
		).toBeInTheDocument();
		expect(screen.getByText("env=prod")).toBeInTheDocument();
		expect(screen.getByText("team=core")).toBeInTheDocument();
	});

	it("renders a non-ASCII host name and label value in full", async () => {
		await openHost(
			makeDetail("сервер-бэкапов.пример", {
				labels: [{ key: "team", value: "Платформа" }],
			}),
		);
		expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent(
			"сервер-бэкапов.пример",
		);
		expect(await screen.findByText("team=Платформа")).toBeInTheDocument();
	});

	it("shows a placeholder tab for ?tab=snapshots", async () => {
		await openHost(makeDetail("web-1"), "?tab=snapshots");
		expect(
			await screen.findByRole("heading", {
				name: "Snapshots will appear here",
			}),
		).toBeInTheDocument();
		expect(screen.getByText("Arrives in a later release")).toBeInTheDocument();
		expect(screen.queryByText("Kernel")).toBeNull();
	});

	it("shows a placeholder tab for ?tab=jobs", async () => {
		await openHost(makeDetail("web-1"), "?tab=jobs");
		expect(
			await screen.findByRole("heading", { name: "Jobs will appear here" }),
		).toBeInTheDocument();
	});

	it("shows an unknown uptime as a dash when the agent reports zero", async () => {
		await openHost(makeDetail("mac-1", { uptime_s: 0 }));
		const label = await screen.findByText("Uptime");
		expect(label.nextElementSibling).toHaveTextContent("—");
	});

	it("shows the jobs illustration on the Jobs tab", async () => {
		await openHost(makeDetail("web-1"), "?tab=jobs");
		await screen.findByRole("heading", { name: "Jobs will appear here" });
		expect(document.querySelector('img[src*="preview-jobs"]')).not.toBeNull();
	});

	it("shows the host tabs illustration on the Snapshots tab", async () => {
		await openHost(makeDetail("web-1"), "?tab=snapshots");
		await screen.findByRole("heading", { name: "Snapshots will appear here" });
		expect(
			document.querySelector('img[src*="preview-host-tabs"]'),
		).not.toBeNull();
	});

	it("falls back to Overview for an unknown tab", async () => {
		await openHost(makeDetail("web-1"), "?tab=bogus");
		expect(await screen.findByText("Kernel")).toBeInTheDocument();
		expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	});

	it("keeps the tab in the search parameters", async () => {
		const { router } = await openHost(makeDetail("web-1"));
		await userEvent.click(screen.getByRole("tab", { name: "Jobs" }));
		await waitFor(() =>
			expect(router.state.location.search).toEqual({ tab: "jobs" }),
		);
		expect(
			await screen.findByRole("heading", { name: "Jobs will appear here" }),
		).toBeInTheDocument();
	});

	it("shows a revoked banner and no actions for a revoked host", async () => {
		await openHost(makeDetail("old-1", { status: "revoked" }));
		expect(screen.getByText(/This host is revoked\./)).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Edit labels" })).toBeNull();
		expect(screen.queryByRole("button", { name: "Revoke host" })).toBeNull();
		expect(screen.getByRole("tab", { name: "Snapshots" })).toBeInTheDocument();
	});

	it("shows Host not found with a way back for an unknown id", async () => {
		server.use(
			http.get("/api/v1/hosts/missing", () =>
				HttpResponse.json(
					{ code: "not_found", message: "no" },
					{ status: 404 },
				),
			),
		);
		useSignedIn();
		renderApp("/hosts/missing");
		expect(
			await screen.findByRole("heading", { level: 1, name: "Host not found" }),
		).toBeInTheDocument();
		expect(screen.getByRole("link", { name: "Back to Hosts" })).toHaveAttribute(
			"href",
			"/hosts",
		);
		expect(screen.queryByText(/Couldn't load this host/)).toBeNull();
	});

	it("shows skeletons while the host loads", async () => {
		const host = makeDetail("web-1");
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, async () => {
				await new Promise((resolve) => setTimeout(resolve, 200));
				return HttpResponse.json(host);
			}),
		);
		useSignedIn();
		renderApp(`/hosts/${host.id}`);
		await waitFor(() =>
			expect(document.querySelector("[data-slot=skeleton]")).not.toBeNull(),
		);
		expect(await screen.findByRole("heading", { level: 1 })).toHaveTextContent(
			"web-1",
		);
	});

	it("stays on the page in the revoked state after revoking", async () => {
		const host = makeDetail("web-1");
		let revoked = false;
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () =>
				HttpResponse.json({ ...host, status: revoked ? "revoked" : "online" }),
			),
			http.post(`/api/v1/hosts/${host.id}/revoke`, () => {
				revoked = true;
				return new HttpResponse(null, { status: 204 });
			}),
		);
		useSignedIn();
		const { router } = renderApp(`/hosts/${host.id}`);
		await userEvent.click(
			await screen.findByRole("button", { name: "Revoke host" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Revoke host" }),
		);
		expect(
			await screen.findByText(/This host is revoked\./),
		).toBeInTheDocument();
		expect(router.state.location.pathname).toBe(`/hosts/${host.id}`);
		expect(
			(await screen.findAllByText('Host "web-1" revoked.')).length,
		).toBeGreaterThan(0);
	});
});
