import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { beforeEach, describe, expect, it } from "vitest";
import type { Host } from "@/api/gen/model";
import { repositoriesQuery } from "@/lib/session";
import {
	apiError,
	renderApp,
	repository,
	useSignedIn,
	useSignedOut,
} from "@/test/app";
import { server } from "@/test/server";

async function openApp(path = "/overview") {
	renderApp(path);
	await screen.findByRole("heading", { level: 1 });
}

function makeHost(hostname: string, over: Partial<Host> = {}): Host {
	return {
		id: `${hostname.replace(/[^a-z0-9]/gi, "").padEnd(8, "0")}-0000-4000-8000-000000000000`,
		hostname,
		os: "linux",
		arch: "arm64",
		distro: null,
		version: "0.1.0",
		status: "online",
		labels: [],
		repository_id: "repo-1",
		enrolled_at: Date.now() - 86_400_000,
		last_seen_at: Date.now() - 20_000,
		cert_not_after: Date.now() + 86_400_000,
		...over,
	};
}

function mockHosts(hosts: Host[]) {
	server.use(http.get("/api/v1/hosts", () => HttpResponse.json(hosts)));
}

async function openPalette(path = "/overview") {
	const user = userEvent.setup();
	const app = renderApp(path);
	await screen.findByRole("heading", { level: 1 });
	await user.click(trigger());
	const dialog = await screen.findByRole("dialog", { name: "Command palette" });
	return { user, dialog, ...app };
}

const trigger = () => screen.getByRole("button", { name: /^Search…/ });
const palette = () => screen.queryByRole("dialog", { name: "Command palette" });

describe("command palette shell", () => {
	beforeEach(() => {
		useSignedIn();
		mockHosts([]);
	});

	it("adds no heading to the page while closed and names its search field when open", async () => {
		const user = userEvent.setup();
		await openApp();
		expect(
			screen.queryByRole("heading", { name: "Command palette" }),
		).toBeNull();
		await user.click(trigger());
		const dialog = await screen.findByRole("dialog", {
			name: "Command palette",
		});
		expect(
			within(dialog).getByRole("combobox", { name: "Command palette" }),
		).toBeInTheDocument();
	});

	it("toggles with Ctrl+K", async () => {
		const user = userEvent.setup();
		await openApp();
		expect(palette()).toBeNull();
		await user.keyboard("{Control>}k{/Control}");
		expect(
			await screen.findByRole("dialog", { name: "Command palette" }),
		).toBeInTheDocument();
		await user.keyboard("{Control>}k{/Control}");
		expect(palette()).toBeNull();
	});

	it("opens with Meta+K from inside an input", async () => {
		const user = userEvent.setup();
		await openApp("/hosts");
		const search = await screen.findByRole("searchbox");
		await user.click(search);
		await user.keyboard("{Meta>}k{/Meta}");
		expect(
			await screen.findByRole("dialog", { name: "Command palette" }),
		).toBeInTheDocument();
	});

	it("filters pages and navigates on Enter", async () => {
		const user = userEvent.setup();
		const { router } = renderApp("/overview");
		await screen.findByRole("heading", { level: 1 });
		await user.click(trigger());
		const dialog = await screen.findByRole("dialog", {
			name: "Command palette",
		});
		await user.keyboard("pol");
		const options = within(dialog).getAllByRole("option");
		expect(options.map((o) => o.textContent)).toEqual([
			"Policies",
			"Create policy",
		]);
		await user.keyboard("{Enter}");
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/policies"),
		);
		expect(palette()).toBeNull();
		await waitFor(() =>
			expect(document.querySelector("h1[data-page-title]")).toHaveFocus(),
		);
	});

	it("finds the repositories page by its sidebar name Settings", async () => {
		const user = userEvent.setup();
		await openApp();
		await user.click(trigger());
		const dialog = await screen.findByRole("dialog", {
			name: "Command palette",
		});
		await user.keyboard("settings");
		const options = within(dialog).getAllByRole("option");
		expect(options).toHaveLength(1);
		expect(options[0]).toHaveTextContent("Repositories");
	});

	it("lists six pages with Soon on the stub", async () => {
		const user = userEvent.setup();
		await openApp();
		await user.click(trigger());
		const dialog = await screen.findByRole("dialog", {
			name: "Command palette",
		});
		const pages = within(dialog).getAllByRole("option").slice(0, 6);
		expect(pages.map((o) => o.textContent)).toEqual([
			"OverviewSoon",
			"Hosts",
			"Policies",
			"Jobs",
			"Restore",
			"Repositories",
		]);
	});

	it("closes on Escape and returns focus to the trigger", async () => {
		const user = userEvent.setup();
		await openApp();
		await user.click(trigger());
		await screen.findByRole("dialog", { name: "Command palette" });
		await user.keyboard("{Escape}");
		expect(palette()).toBeNull();
		await waitFor(() => expect(trigger()).toHaveFocus());
	});
});

const optionTexts = (dialog: HTMLElement, prefix: string) =>
	within(dialog)
		.queryAllByRole("option")
		.map((o) => o.textContent ?? "")
		.filter((text) => text.startsWith(prefix));

describe("command palette hosts and repositories", () => {
	beforeEach(() => useSignedIn());

	it("finds a host by label and by part of its name", async () => {
		mockHosts([
			makeHost("web-1", { labels: [{ key: "env", value: "prod" }] }),
			makeHost("db-1", { labels: [{ key: "env", value: "dev" }] }),
		]);
		const { user, dialog } = await openPalette();
		await screen.findByText("web-1");
		await user.keyboard("env=prod");
		expect(optionTexts(dialog, "web-1")).toHaveLength(1);
		expect(optionTexts(dialog, "db-1")).toHaveLength(0);
		await user.clear(within(dialog).getByRole("combobox"));
		await user.keyboard("DB");
		expect(optionTexts(dialog, "db-1")).toHaveLength(1);
		expect(optionTexts(dialog, "web-1")).toHaveLength(0);
	});

	it("shows at most 8 hosts with online ones first", async () => {
		mockHosts([
			...[1, 2, 3, 4, 5, 6].map((n) =>
				makeHost(`node-a${n}`, { status: "offline" }),
			),
			...[1, 2, 3, 4, 5, 6].map((n) => makeHost(`node-z${n}`)),
		]);
		const { dialog } = await openPalette();
		await screen.findByText("node-z1");
		const names = optionTexts(dialog, "node-").map(
			(text) => /^node-\w\d/.exec(text)?.[0],
		);
		expect(names).toEqual([
			"node-z1",
			"node-z2",
			"node-z3",
			"node-z4",
			"node-z5",
			"node-z6",
			"node-a1",
			"node-a2",
		]);
	});

	it("hides revoked hosts until a query matches them", async () => {
		mockHosts([
			makeHost("live-box"),
			makeHost("old-box", { status: "revoked" }),
		]);
		const { user, dialog } = await openPalette();
		await screen.findByText("live-box");
		expect(optionTexts(dialog, "old-box")).toHaveLength(0);
		await user.keyboard("old");
		const [revoked] = optionTexts(dialog, "old-box");
		expect(revoked).toContain("Revoked");
	});

	it("matches non-ASCII host names and label values", async () => {
		mockHosts([
			makeHost("сервер-бэкапов.пример"),
			makeHost("plain", { labels: [{ key: "team", value: "Платформа" }] }),
		]);
		const { user, dialog } = await openPalette();
		await screen.findByText("plain");
		await user.keyboard("СЕРВЕР");
		expect(optionTexts(dialog, "сервер-бэкапов.пример")).toHaveLength(1);
		expect(optionTexts(dialog, "plain")).toHaveLength(0);
		await user.clear(within(dialog).getByRole("combobox"));
		await user.keyboard("платформа");
		expect(optionTexts(dialog, "plain")).toHaveLength(1);
		expect(within(dialog).getByText(/team=Платформа/)).toHaveClass("truncate");
	});

	it("opens the host page from a host item", async () => {
		const host = makeHost("web-1");
		mockHosts([host]);
		const { user, router } = await openPalette();
		await user.click(await screen.findByRole("option", { name: /web-1/ }));
		expect(router.state.location.pathname).toBe(`/hosts/${host.id}`);
		expect(palette()).toBeNull();
	});

	it("finds a repository by name and by location", async () => {
		mockHosts([]);
		const { user, dialog, router } = await openPalette();
		await screen.findByRole("option", { name: /main/ });
		await user.keyboard("bucket/pre");
		const options = optionTexts(dialog, "main");
		expect(options).toEqual(["mains3://bucket/prefix"]);
		await user.clear(within(dialog).getByRole("combobox"));
		await user.keyboard("MAIN");
		await user.click(screen.getByRole("option", { name: /main/ }));
		expect(router.state.location.pathname).toBe(
			`/settings/repositories/${repository.id}`,
		);
	});

	it("opens the cleanup card from Free up space, also when searching cleanup", async () => {
		mockHosts([]);
		const { user, router } = await openPalette();
		await user.keyboard("cleanup");
		await user.click(screen.getByRole("option", { name: "Free up space" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe(
				`/settings/repositories/${repository.id}`,
			),
		);
		expect(router.state.location.hash).toBe("free-up-space");
	});

	it("matches cleanup keywords by word start only", async () => {
		mockHosts([]);
		const { user, dialog } = await openPalette();
		await user.keyboard("up c");
		expect(optionTexts(dialog, "Free up space")).toHaveLength(0);
		await user.clear(within(dialog).getByRole("combobox"));
		await user.keyboard("garb");
		expect(optionTexts(dialog, "Free up space")).toHaveLength(1);
	});

	it("opens the Add host dialog when a repository exists", async () => {
		mockHosts([]);
		const { user, router } = await openPalette();
		await user.click(screen.getByRole("option", { name: "Add host" }));
		expect(
			await screen.findByRole("dialog", { name: "Add host" }),
		).toBeInTheDocument();
		expect(router.state.location.pathname).toBe("/hosts");
	});

	it("opens the new policy page from Create policy", async () => {
		mockHosts([]);
		const { user, router } = await openPalette();
		await user.click(screen.getByRole("option", { name: "Create policy" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/policies/new"),
		);
		expect(palette()).toBeNull();
	});

	it("palette offers api tokens", async () => {
		mockHosts([]);
		server.use(http.get("/api/v1/tokens", () => HttpResponse.json([])));
		const { user, dialog, router } = await openPalette();
		await user.keyboard("token");
		expect(
			within(dialog)
				.getAllByRole("option")
				.map((o) => o.textContent),
		).toEqual(["API tokens", "Create API token"]);

		await user.click(screen.getByRole("option", { name: "Create API token" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/tokens"),
		);
		expect(
			await screen.findByRole("dialog", { name: "Create API token" }),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(router.state.location.search).not.toHaveProperty("create"),
		);
	});

	it("sends Add host to the welcome page when there are no repositories", async () => {
		mockHosts([]);
		const { user, router, queryClient } = await openPalette();
		queryClient.setQueryData(repositoriesQuery.queryKey, []);
		await user.click(screen.getByRole("option", { name: "Add host" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/welcome"),
		);
	});

	it("switches the theme", async () => {
		mockHosts([]);
		const { user } = await openPalette();
		await user.click(
			screen.getByRole("option", { name: "Switch to dark theme" }),
		);
		expect(document.documentElement).toHaveClass("dark");
	});

	it("signs out through the shared hook", async () => {
		mockHosts([]);
		let loggedOut = false;
		server.use(
			http.post("/api/v1/auth/logout", () => {
				loggedOut = true;
				useSignedOut();
				return new HttpResponse(null, { status: 204 });
			}),
		);
		const { user, router } = await openPalette();
		await user.click(screen.getByRole("option", { name: "Sign out" }));
		await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
		expect(loggedOut).toBe(true);
	});

	it("says so when nothing matches", async () => {
		mockHosts([]);
		const { user } = await openPalette();
		await screen.findByRole("option", { name: /main/ });
		await user.keyboard("zzz");
		expect(
			await screen.findByText(
				'No results for "zzz". Try a host name or a label such as env=prod.',
			),
		).toBeInTheDocument();
	});

	it("shows one disabled item when the host list fails", async () => {
		server.use(
			http.get("/api/v1/hosts", () => apiError(500, "internal", "boom")),
		);
		await openPalette();
		const item = await screen.findByRole("option", {
			name: "Couldn't load hosts",
		});
		expect(item).toHaveAttribute("aria-disabled", "true");
	});

	it("shows three skeleton rows while hosts load", async () => {
		server.use(
			http.get("/api/v1/hosts", () => new Promise<Response>(() => {})),
		);
		const { dialog } = await openPalette();
		expect(dialog.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(3);
	});
});
