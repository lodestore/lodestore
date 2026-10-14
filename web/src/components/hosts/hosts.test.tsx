import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { Host, HostDetail } from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;

globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};

function makeHost(name: string, over: Partial<Host> = {}): Host {
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
		...over,
	};
}

function mockHosts(list: Host[] | (() => Host[])) {
	server.use(
		http.get("/api/v1/hosts", () =>
			HttpResponse.json(typeof list === "function" ? list() : list),
		),
	);
}

async function openHosts() {
	useSignedIn();
	renderApp("/hosts");
	await screen.findByRole("heading", { name: "Hosts" });
}

describe("Hosts page", () => {
	it("lists hosts with status, version and platform", async () => {
		mockHosts([
			makeHost("web-1"),
			makeHost("db-1", { status: "offline", version: "0.1.0" }),
		]);
		await openHosts();

		expect(await screen.findByText("web-1")).toBeInTheDocument();
		expect(screen.getByText("db-1")).toBeInTheDocument();
		const online = screen.getByText("Online").closest("[data-status]");
		expect(online).toHaveAttribute("data-status", "online");
		expect(online?.querySelector("svg")).not.toBeNull();
		const offline = screen.getByText("Offline").closest("[data-status]");
		expect(offline).toHaveAttribute("data-status", "offline");
		expect(offline?.querySelector("svg")).not.toBeNull();
		expect(screen.getAllByText("linux / arm64")).toHaveLength(2);
		expect(screen.getAllByText("0.1.0").length).toBeGreaterThan(0);
	});

	it("shows a host added through the dialog without a reload", async () => {
		let hosts = [makeHost("web-1"), makeHost("db-1", { status: "offline" })];
		mockHosts(() => hosts);
		server.use(
			http.post("/api/v1/join-tokens", () =>
				HttpResponse.json(
					{
						id: "token-1",
						token: "lode1.secret",
						command:
							"curl -fsSL http://localhost/install.sh | sh -s -- --join x",
						expires_at: Date.now() + 3_600_000,
						server_time: Date.now(),
					},
					{ status: 201 },
				),
			),
			http.get("/api/v1/join-tokens/token-1", () => {
				hosts = [...hosts, makeHost("new-host")];
				return HttpResponse.json({
					status: "used",
					expires_at: Date.now() + 3_600_000,
					server_time: Date.now(),
					host: {
						id: "h-new",
						hostname: "new-host",
						version: "0.1.0",
						status: "online",
					},
				});
			}),
		);
		await openHosts();
		await screen.findByText("web-1");
		expect(screen.queryByText("new-host")).toBeNull();

		const user = userEvent.setup();
		await user.click(screen.getByRole("button", { name: "Add host" }));
		const dialog = await screen.findByRole("dialog", { name: "Add host" });
		await user.click(
			await within(dialog).findByRole("button", { name: "Generate token" }),
		);
		expect(
			await within(dialog).findByText("new-host is online. Version 0.1.0."),
		).toBeInTheDocument();
		await user.click(
			within(dialog).getByRole("button", { name: "Finish host setup" }),
		);

		const table = await screen.findByRole("table");
		expect(await within(table).findByText("new-host")).toBeInTheDocument();
		expect(screen.queryByRole("dialog")).toBeNull();
	});
});

function bodyRows() {
	return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

function firstRowName() {
	return within(bodyRows()[0]).getAllByRole("button")[0].textContent;
}

function many(n: number, over: (i: number) => Partial<Host> = () => ({})) {
	return Array.from({ length: n }, (_, i) =>
		makeHost(`host-${String(i).padStart(3, "0")}`, over(i)),
	);
}

describe("Hosts table", () => {
	it("paginates 50 rows per page", async () => {
		mockHosts(many(212));
		await openHosts();
		expect(await screen.findByText("Showing 1-50 of 212")).toBeInTheDocument();
		expect(bodyRows()).toHaveLength(50);

		await userEvent.click(screen.getByRole("button", { name: "Next" }));
		expect(
			await screen.findByText("Showing 51-100 of 212"),
		).toBeInTheDocument();
		expect(firstRowName()).toBe("host-050");
		expect(screen.getByRole("button", { name: "Previous" })).toBeEnabled();
	});

	it("searches by label across all pages", async () => {
		mockHosts(
			many(212, (i) => ({
				labels: [{ key: "env", value: i % 3 === 0 ? "prod" : "dev" }],
			})),
		);
		await openHosts();
		await screen.findByText("Showing 1-50 of 212");

		await userEvent.type(screen.getByRole("searchbox"), "env=prod");
		expect(await screen.findByText("Showing 1-50 of 71")).toBeInTheDocument();
		for (const row of bodyRows()) {
			expect(within(row).getByText("env=prod")).toBeInTheDocument();
		}

		await userEvent.click(screen.getByRole("button", { name: "Next" }));
		expect(await screen.findByText("Showing 51-71 of 71")).toBeInTheDocument();
	});

	it("sorts by host name in both directions", async () => {
		mockHosts([makeHost("b-host"), makeHost("a-host"), makeHost("c-host")]);
		await openHosts();
		await screen.findByText("a-host");
		expect(firstRowName()).toBe("a-host");

		const user = userEvent.setup();
		const header = screen.getByRole("columnheader", { name: /Host/ });
		await user.click(within(header).getByRole("button", { name: "Host" }));
		expect(header).toHaveAttribute("aria-sort", "ascending");
		expect(firstRowName()).toBe("a-host");

		await user.click(within(header).getByRole("button", { name: "Host" }));
		expect(header).toHaveAttribute("aria-sort", "descending");
		expect(firstRowName()).toBe("c-host");
	});

	it("puts online hosts first by default", async () => {
		mockHosts([
			makeHost("a-offline", { status: "offline" }),
			makeHost("z-online"),
		]);
		await openHosts();
		await screen.findByText("z-online");
		expect(firstRowName()).toBe("z-online");
	});

	it("shows three label chips and a counter with the rest", async () => {
		mockHosts([
			makeHost("web-1", {
				labels: ["a", "b", "c", "d", "e"].map((key) => ({ key, value: "1" })),
			}),
		]);
		await openHosts();
		await screen.findByText("web-1");
		const row = bodyRows()[0];
		expect(within(row).getByText("a=1")).toBeInTheDocument();
		expect(within(row).getByText("c=1")).toBeInTheDocument();
		expect(within(row).queryByText("d=1")).toBeNull();
		await userEvent.hover(within(row).getByText("+2"));
		const tip = await screen.findByRole("tooltip");
		expect(tip).toHaveTextContent("d=1");
		expect(tip).toHaveTextContent("e=1");
	});

	it("flags an agent version that differs from the server", async () => {
		mockHosts([makeHost("old-1", { version: "0.0.9" }), makeHost("ok-1")]);
		await openHosts();
		await screen.findByText("old-1");
		const warning = screen.getAllByRole("img", {
			name: "Agent version differs from the server (v0.1.0)",
		});
		expect(warning).toHaveLength(1);
		await userEvent.hover(warning[0]);
		expect(await screen.findByRole("tooltip")).toHaveTextContent(
			"Agent version differs from the server (v0.1.0)",
		);
	});

	it("shows certificate expiry with warning and expired states", async () => {
		mockHosts([
			makeHost("one", { cert_not_after: Date.now() + DAY }),
			makeHost("ten", { cert_not_after: Date.now() + 10 * DAY }),
			makeHost("fine", { cert_not_after: Date.now() + 60 * DAY }),
			makeHost("gone", { cert_not_after: Date.now() - DAY }),
			makeHost("idle", {
				status: "offline",
				cert_not_after: Date.now() + 5 * DAY,
			}),
		]);
		await openHosts();
		await screen.findByText("one");
		expect(screen.getByText("Expires in 1 day")).toBeInTheDocument();
		expect(screen.getByText("Expires in 10 days")).toHaveClass("text-warning");
		expect(screen.getByText("Expires in 60 days")).not.toHaveClass(
			"text-warning",
		);
		expect(screen.getByText("Expired")).toHaveClass("text-destructive");
		expect(screen.getByText("Expires in 5 days")).not.toHaveClass(
			"text-warning",
		);
	});

	it("shows the empty state with an add button", async () => {
		mockHosts([]);
		await openHosts();
		expect(await screen.findByText("No hosts yet")).toBeInTheDocument();
		expect(
			screen.getByText(/Add a host to start protecting it/),
		).toBeInTheDocument();
		expect(screen.getAllByRole("button", { name: "Add host" })).toHaveLength(2);
	});

	it("shows an empty revoked filter", async () => {
		mockHosts([makeHost("web-1")]);
		await openHosts();
		await screen.findByText("web-1");
		await userEvent.click(screen.getByRole("radio", { name: "Revoked" }));
		expect(await screen.findByText("No revoked hosts")).toBeInTheDocument();
		expect(screen.getAllByRole("button", { name: "Add host" })).toHaveLength(1);
	});

	it("shows a search-specific empty state without a button", async () => {
		mockHosts([makeHost("web-1")]);
		await openHosts();
		await screen.findByText("web-1");
		await userEvent.type(screen.getByRole("searchbox"), "zzz");
		expect(await screen.findByText('No hosts match "zzz"')).toBeInTheDocument();
		expect(screen.getAllByRole("button", { name: "Add host" })).toHaveLength(1);
	});

	it("shows skeleton rows while loading", async () => {
		server.use(
			http.get("/api/v1/hosts", async () => {
				await new Promise((resolve) => setTimeout(resolve, 300));
				return HttpResponse.json([makeHost("late-1")]);
			}),
		);
		await openHosts();
		expect(screen.getByRole("table").querySelectorAll("tbody tr")).toHaveLength(
			5,
		);
		expect(await screen.findByText("late-1")).toBeInTheDocument();
	});

	it("reports a load failure and retries", async () => {
		server.use(
			http.get("/api/v1/hosts", () =>
				HttpResponse.json({ code: "x", message: "boom" }, { status: 500 }),
			),
		);
		await openHosts();
		expect(await screen.findByText(/Couldn't load hosts/)).toBeInTheDocument();
		mockHosts([makeHost("back-1")]);
		await userEvent.click(
			screen.getByRole("button", { name: "Retry loading" }),
		);
		expect(await screen.findByText("back-1")).toBeInTheDocument();
	});

	it("renders a non-ASCII host name in full in the tooltip", async () => {
		mockHosts([makeHost("сервер-бэкапов.пример")]);
		await openHosts();
		const name = await screen.findByRole("button", {
			name: "сервер-бэкапов.пример",
		});
		await userEvent.hover(name);
		expect(await screen.findByRole("tooltip")).toHaveTextContent(
			"сервер-бэкапов.пример",
		);
	});

	it("renders a non-ASCII label value in full in the tooltip", async () => {
		mockHosts([
			makeHost("web-1", { labels: [{ key: "team", value: "Платформа" }] }),
		]);
		await openHosts();
		await userEvent.hover(await screen.findByText("team=Платформа"));
		expect(await screen.findByRole("tooltip")).toHaveTextContent(
			"team=Платформа",
		);
	});
});

function makeDetail(host: Host, over: Partial<HostDetail> = {}): HostDetail {
	return {
		...host,
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

async function openMenu(name: string, item: string) {
	const user = userEvent.setup();
	await user.click(
		await screen.findByRole("button", { name: `Actions for ${name}` }),
	);
	await user.click(await screen.findByRole("menuitem", { name: item }));
	return user;
}

async function waitForRowGone(name: string) {
	await waitFor(() =>
		expect(
			within(screen.getByRole("table")).queryByRole("button", { name }),
		).toBeNull(),
	);
}

describe("Host details", () => {
	it("limits filesystems to 20 rows and expands on request", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		mockDetail(
			makeDetail(host, {
				mounts: Array.from({ length: 60 }, (_, i) => ({
					path: `/mnt/fs${String(i).padStart(2, "0")}`,
					fs_type: "ext4",
					total_bytes: 100 * 1024 ** 3,
					free_bytes: (100 - i) * 1024 ** 3,
				})),
			}),
		);
		await openHosts();
		await userEvent.click(await screen.findByRole("button", { name: "web-1" }));

		const sheet = await screen.findByRole("dialog", { name: "web-1" });
		expect(await within(sheet).findByText("Storage")).toBeInTheDocument();
		expect(within(sheet).getAllByRole("row")).toHaveLength(21);
		expect(within(sheet).getAllByRole("row")[1]).toHaveTextContent("/mnt/fs59");
		expect(within(sheet).getAllByRole("row")[1]).toHaveTextContent("59%");

		await userEvent.click(
			within(sheet).getByRole("button", { name: "Show all 60 filesystems" }),
		);
		expect(within(sheet).getAllByRole("row")).toHaveLength(61);
		expect(
			within(sheet).queryByRole("button", { name: /Show all/ }),
		).toBeNull();
		expect(within(sheet).getByText("Overview")).toBeInTheDocument();
		expect(within(sheet).getByText("main")).toBeInTheDocument();
	});

	it("shows a dash for missing facts and hides empty sections", async () => {
		const host = makeHost("fresh-1");
		mockHosts([host]);
		mockDetail(makeDetail(host, { kernel: null, uptime_s: null }));
		await openHosts();
		await userEvent.click(
			await screen.findByRole("button", { name: "fresh-1" }),
		);

		const sheet = await screen.findByRole("dialog", { name: "fresh-1" });
		expect(await within(sheet).findByText("Kernel")).toBeInTheDocument();
		expect(within(sheet).getAllByText("—").length).toBeGreaterThanOrEqual(3);
		expect(within(sheet).queryByText("Storage")).toBeNull();
	});

	it("reports a details failure and retries", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		server.use(
			http.get(`/api/v1/hosts/${host.id}`, () =>
				HttpResponse.json({ code: "x", message: "boom" }, { status: 500 }),
			),
		);
		await openHosts();
		await userEvent.click(await screen.findByRole("button", { name: "web-1" }));
		expect(
			await screen.findByText(/Couldn't load host details\./),
		).toBeInTheDocument();

		mockDetail(makeDetail(host));
		await userEvent.click(
			screen.getByRole("button", { name: "Retry loading" }),
		);
		expect(await screen.findByText("Kernel")).toBeInTheDocument();
	});
});

describe("Edit labels", () => {
	it("starts with one focused blank row and saves an empty set", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		let body: unknown;
		server.use(
			http.put(`/api/v1/hosts/${host.id}/labels`, async ({ request }) => {
				body = await request.json();
				return HttpResponse.json({});
			}),
		);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Edit labels");

		const dialog = await screen.findByRole("dialog", { name: "Edit labels" });
		expect(
			within(dialog).getAllByRole("textbox", { name: "Key" }),
		).toHaveLength(1);
		expect(within(dialog).getByRole("textbox", { name: "Key" })).toHaveFocus();

		await user.click(
			within(dialog).getByRole("button", { name: "Save labels" }),
		);
		expect(await screen.findByText("Labels saved.")).toBeInTheDocument();
		expect(body).toEqual({ labels: [] });
		expect(screen.queryByRole("dialog")).toBeNull();
	});

	it("blocks saving a key with a space", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Edit labels");

		const dialog = await screen.findByRole("dialog", { name: "Edit labels" });
		await user.type(
			within(dialog).getByRole("textbox", { name: "Key" }),
			"a b",
		);
		expect(
			within(dialog).getByRole("button", { name: "Save labels" }),
		).toBeDisabled();
	});

	it("shows the server message on a rejected save and keeps the dialog", async () => {
		const host = makeHost("web-1", { labels: [{ key: "env", value: "prod" }] });
		mockHosts([host]);
		server.use(
			http.put(`/api/v1/hosts/${host.id}/labels`, () =>
				HttpResponse.json(
					{ code: "invalid", message: "label key reserved" },
					{ status: 422 },
				),
			),
		);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Edit labels");

		const dialog = await screen.findByRole("dialog", { name: "Edit labels" });
		await user.click(
			within(dialog).getByRole("button", { name: "Save labels" }),
		);
		expect(await within(dialog).findByRole("alert")).toHaveTextContent(
			"label key reserved",
		);
	});

	it("closes without saving on discard", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Edit labels");

		const dialog = await screen.findByRole("dialog", { name: "Edit labels" });
		await user.click(
			within(dialog).getByRole("button", { name: "Discard changes" }),
		);
		expect(screen.queryByRole("dialog")).toBeNull();
	});
});

describe("Revoke host", () => {
	it("warns about the shared key and links to the repository", async () => {
		mockHosts([makeHost("web-1")]);
		await openHosts();
		await screen.findByText("web-1");
		await openMenu("web-1", "Revoke host");

		const dialog = await screen.findByRole("alertdialog", {
			name: 'Revoke host "web-1"?',
		});
		expect(dialog).toHaveTextContent("shared by all hosts of this repository");
		expect(
			within(dialog).getByRole("link", { name: "Replace the agent key" }),
		).toHaveAttribute("href", "/settings/repositories/repo-1");
	});

	it("disables both buttons while the request runs", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		server.use(
			http.post(`/api/v1/hosts/${host.id}/revoke`, async () => {
				await new Promise((resolve) => setTimeout(resolve, 300));
				return new HttpResponse(null, { status: 204 });
			}),
		);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Revoke host");

		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Revoke host" }),
		);
		expect(
			within(dialog).getByRole("button", { name: "Keep host" }),
		).toBeDisabled();
		expect(
			within(dialog).getByRole("button", { name: /Revoke host/ }),
		).toBeDisabled();
		await screen.findAllByText('Host "web-1" revoked.');
	});

	it("keeps the dialog open with an alert when the request fails", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		server.use(
			http.post(`/api/v1/hosts/${host.id}/revoke`, () =>
				HttpResponse.json(
					{ code: "x", message: "database busy" },
					{ status: 500 },
				),
			),
		);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Revoke host");

		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Revoke host" }),
		);
		expect(await within(dialog).findByRole("alert")).toHaveTextContent(
			"database busy",
		);
		expect(
			within(dialog).getByRole("button", { name: "Keep host" }),
		).toBeEnabled();
	});

	it("moves the host to the revoked filter on success", async () => {
		let revoked = false;
		const web = () =>
			makeHost("web-1", { status: revoked ? "revoked" : "online" });
		mockHosts(() => [web(), makeHost("db-1")]);
		server.use(
			http.post(`/api/v1/hosts/${web().id}/revoke`, () => {
				revoked = true;
				return new HttpResponse(null, { status: 204 });
			}),
		);
		await openHosts();
		await screen.findByText("web-1");
		const user = await openMenu("web-1", "Revoke host");

		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Revoke host" }),
		);
		expect(
			(await screen.findAllByText('Host "web-1" revoked.')).length,
		).toBeGreaterThan(0);
		await waitForRowGone("web-1");
		expect(screen.queryByRole("alertdialog")).toBeNull();

		await user.click(screen.getByRole("radio", { name: "Revoked" }));
		expect(await screen.findByText("web-1")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Actions for web-1" }));
		expect(
			await screen.findByRole("menuitem", { name: "View details" }),
		).toBeInTheDocument();
		expect(screen.queryByRole("menuitem", { name: "Revoke host" })).toBeNull();
		expect(screen.queryByRole("menuitem", { name: "Edit labels" })).toBeNull();
	});
});
