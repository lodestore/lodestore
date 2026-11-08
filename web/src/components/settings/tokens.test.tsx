import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { ApiTokenItem } from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;
const SECRET = "lsk_Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZg";

function makeToken(
	name: string,
	over: Partial<ApiTokenItem> = {},
): ApiTokenItem {
	return {
		id: `id-${name}`,
		name,
		prefix: "lsk_a1b2c3",
		scopes: ["policies:read"],
		created_at: Date.now() - DAY,
		created_by: "admin",
		expires_at: Date.now() + 60 * DAY,
		last_used_at: null,
		revoked_at: null,
		...over,
	};
}

function mockTokens(initial: ApiTokenItem[]) {
	let items = initial;
	const calls = { revoked: [] as string[], created: [] as unknown[] };
	server.use(
		http.get("/api/v1/tokens", ({ request }) => {
			const state = new URL(request.url).searchParams.get("state");
			return HttpResponse.json(
				items.filter((t) =>
					state === "revoked" ? t.revoked_at != null : t.revoked_at == null,
				),
			);
		}),
		http.post("/api/v1/tokens", async ({ request }) => {
			const body = (await request.json()) as {
				name: string;
				scopes: ApiTokenItem["scopes"];
			};
			calls.created.push(body);
			const item = makeToken(body.name, { scopes: body.scopes });
			items = [item, ...items];
			return HttpResponse.json({ token: SECRET, item }, { status: 201 });
		}),
		http.post("/api/v1/tokens/:id/revoke", ({ params }) => {
			calls.revoked.push(String(params.id));
			items = items.map((t) =>
				t.id === params.id ? { ...t, revoked_at: Date.now() } : t,
			);
			return new HttpResponse(null, { status: 204 });
		}),
	);
	return calls;
}

async function openTokens(path = "/settings/tokens") {
	useSignedIn();
	const app = renderApp(path);
	await screen.findByRole("heading", {
		name: "API tokens",
		level: 1,
		hidden: true,
	});
	return app;
}

const createButton = () =>
	screen.getAllByRole("button", { name: "Create token" })[0];

describe("API tokens: create", () => {
	it("create token shows secret once", async () => {
		const calls = mockTokens([]);
		const user = userEvent.setup();
		await openTokens();

		await user.click(createButton());
		const dialog = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		await user.type(within(dialog).getByLabelText("Name"), "ci-gitops");
		await user.click(
			within(dialog).getByRole("checkbox", { name: "policies:read" }),
		);
		await user.click(
			within(dialog).getByRole("checkbox", { name: "policies:write" }),
		);
		await user.click(
			within(dialog).getByRole("button", { name: "Create token" }),
		);

		const secret = await screen.findByRole("dialog", { name: "Token created" });
		expect(calls.created).toEqual([
			{
				name: "ci-gitops",
				scopes: ["policies:read", "policies:write"],
				expires_in_days: 90,
			},
		]);
		expect(
			within(secret).getByText("Copy this token now. It is not shown again."),
		).toBeInTheDocument();
		const sensitive = secret.querySelector("[data-sensitive]");
		expect(sensitive).toHaveTextContent(SECRET);

		const copyButtons = within(secret).getAllByRole("button", {
			name: "Copy token",
		});
		expect(copyButtons).toHaveLength(2);
		await user.click(copyButtons[1]);
		await waitFor(() =>
			expect(
				within(secret)
					.getAllByRole("status")
					.some((el) => el.textContent === "Copied"),
			).toBe(true),
		);

		await userEvent.setup({ pointerEventsCheck: 0 }).click(document.body);
		expect(
			screen.getByRole("dialog", { name: "Token created" }),
		).toBeInTheDocument();

		await user.click(
			within(secret).getByRole("button", { name: "I've copied the token" }),
		);
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.body.textContent).not.toContain(SECRET);
		expect(JSON.stringify({ ...localStorage })).not.toContain(SECRET);
		expect(JSON.stringify({ ...sessionStorage })).not.toContain(SECRET);
		expect(window.location.href).not.toContain(SECRET);

		const row = (await screen.findByText("ci-gitops")).closest("tr");
		expect(row).not.toBeNull();
		await waitFor(() => expect(row).toHaveFocus());
	});

	it("create token validates name and scopes", async () => {
		const calls = mockTokens([]);
		const user = userEvent.setup();
		await openTokens();

		await user.click(createButton());
		const dialog = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		await user.click(
			within(dialog).getByRole("button", { name: "Create token" }),
		);
		expect(
			within(dialog).getByText("Enter a name for the token."),
		).toBeInTheDocument();
		expect(
			within(dialog).getByText("Choose at least one permission."),
		).toBeInTheDocument();
		expect(within(dialog).getByLabelText("Name")).toBeInvalid();
		expect(calls.created).toEqual([]);

		await user.type(within(dialog).getByLabelText("Name"), "ci");
		await user.click(
			within(dialog).getByRole("checkbox", { name: "restore:download" }),
		);
		expect(
			within(dialog).queryByText("Enter a name for the token."),
		).toBeNull();
		expect(
			within(dialog).queryByText("Choose at least one permission."),
		).toBeNull();
		expect(
			within(dialog).getByText(
				"For CI that applies policies, choose policies:read and policies:write. Policies with retention also need snapshots:delete, and policies with hooks need hooks:write. For Prometheus, choose metrics:read.",
			),
		).toBeInTheDocument();
		expect(within(dialog).getByText("Deletes data")).toBeInTheDocument();
	});

	it("offers the metrics scope", async () => {
		const user = userEvent.setup();
		mockTokens([]);
		await openTokens();
		expect(
			screen.getByText(
				/A token cannot manage tokens, repositories, notification channels or the admin password\./,
			),
		).toBeInTheDocument();
		await user.click(createButton());
		const dialog = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		const names = within(dialog)
			.getAllByRole("checkbox")
			.map((box) => box.getAttribute("id"))
			.filter((id) => id?.includes(":"));
		const scopes = names.map((id) => id?.split("-").pop());
		expect(scopes.indexOf("metrics:read")).toBe(scopes.indexOf("jobs:run") + 1);
		expect(scopes.indexOf("metrics:read")).toBeLessThan(
			scopes.indexOf("snapshots:read"),
		);
		const row = within(dialog)
			.getByRole("checkbox", { name: "metrics:read" })
			.closest("div") as HTMLElement;
		expect(within(row).getByText("For Prometheus")).toBeVisible();
		expect(within(row).queryByText("Deletes data")).toBeNull();
	});

	it("warns on the scopes that write over hosts", async () => {
		const calls = mockTokens([]);
		const user = userEvent.setup();
		await openTokens();
		await user.click(createButton());
		const dialog = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		const rowOf = (name: string) =>
			within(dialog)
				.getByRole("checkbox", { name })
				.closest("div") as HTMLElement;
		expect(
			within(rowOf("restore:in-place")).getByText("Overwrites files on hosts"),
		).toBeVisible();
		expect(
			within(rowOf("hooks:write")).getByText("Runs commands as root on hosts"),
		).toBeVisible();
		for (const safe of ["restore:download", "restore:host"]) {
			expect(within(rowOf(safe)).queryByText(/hosts$|Deletes data/)).toBeNull();
		}

		await user.type(within(dialog).getByLabelText("Name"), "ci");
		await user.click(
			within(dialog).getByRole("checkbox", { name: "restore:host" }),
		);
		await user.click(
			within(dialog).getByRole("checkbox", { name: "hooks:write" }),
		);
		await user.click(
			within(dialog).getByRole("button", { name: "Create token" }),
		);
		await screen.findByRole("dialog", { name: "Token created" });
		expect(calls.created).toMatchObject([
			{ scopes: ["hooks:write", "restore:host"] },
		]);
	});

	it("offers the alerts scope after hosts:write", async () => {
		const user = userEvent.setup();
		mockTokens([]);
		await openTokens();
		await user.click(createButton());
		const dialog = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		const scopes = within(dialog)
			.getAllByRole("checkbox")
			.map((box) => box.getAttribute("id"))
			.filter((id) => id?.includes(":"))
			.map((id) => id?.split("-").pop());
		expect(scopes.indexOf("alerts:write")).toBe(
			scopes.indexOf("hosts:write") + 1,
		);
		await user.click(
			within(dialog).getByRole("checkbox", { name: "alerts:write" }),
		);
		expect(
			within(dialog).getByRole("checkbox", { name: "alerts:write" }),
		).toBeChecked();
	});

	it("create param opens dialog", async () => {
		mockTokens([]);
		const { router } = await openTokens("/settings/tokens?create=1");
		expect(
			await screen.findByRole("dialog", { name: "Create API token" }),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(router.state.location.search).not.toHaveProperty("create"),
		);
	});
});

describe("API tokens: table", () => {
	it("table renders scopes and expiry", async () => {
		mockTokens([
			makeToken("deploy", {
				scopes: ["hosts:read", "hosts:write", "jobs:run", "restore:download"],
				expires_at: Date.now() + 5 * DAY - 60_000,
				last_used_at: Date.now() - 3_600_000,
			}),
			makeToken("forever", { expires_at: null }),
			makeToken("stale", { expires_at: Date.now() - DAY }),
		]);
		await openTokens();

		await screen.findByText("deploy");
		const table = screen.getByRole("table");
		const deploy = within(table).getByText("deploy").closest("tr");
		if (!deploy) throw new Error("row missing");
		expect(within(deploy).getByText("lsk_a1b2c3…")).toBeInTheDocument();
		expect(within(deploy).getByText("hosts:read")).toBeInTheDocument();
		expect(within(deploy).getByText("hosts:write")).toBeInTheDocument();
		expect(within(deploy).queryByText("jobs:run")).toBeNull();
		expect(within(deploy).getByText("+2")).toBeInTheDocument();
		expect(
			within(deploy).getByRole("group", {
				name: "Scopes: hosts:read, hosts:write, jobs:run, restore:download",
			}),
		).toBeInTheDocument();
		const expiring = within(deploy).getByText("In 5 days");
		expect(expiring).toHaveClass("text-warning");
		expect(expiring.querySelector("svg")).not.toBeNull();
		expect(within(deploy).getByText("1 hour ago")).toBeInTheDocument();

		const forever = within(table).getByText("forever").closest("tr");
		if (!forever) throw new Error("row missing");
		expect(within(forever).getByText("Never expires")).toBeInTheDocument();
		expect(within(forever).getByText("Never used")).toBeInTheDocument();

		const stale = within(table).getByText("stale").closest("tr");
		if (!stale) throw new Error("row missing");
		expect(within(stale).getByText("Expired")).toBeInTheDocument();
	});
});

describe("API tokens: revoke, filter and states", () => {
	it("revoke token flow", async () => {
		const calls = mockTokens([makeToken("ci"), makeToken("other")]);
		const user = userEvent.setup();
		await openTokens();

		await user.click(
			await screen.findByRole("button", { name: "Actions for ci" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Revoke token" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: 'Revoke token "ci"?',
		});
		expect(
			within(dialog).getByText(
				"Anything that uses it stops working immediately. This cannot be undone.",
			),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(
				within(dialog).getByRole("button", { name: "Keep token" }),
			).toHaveFocus(),
		);

		await user.click(
			within(dialog).getByRole("button", { name: "Revoke token" }),
		);
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(calls.revoked).toEqual(["id-ci"]);
		expect(
			(await screen.findAllByText('Token "ci" revoked.')).length,
		).toBeGreaterThan(0);
		await waitFor(() =>
			expect(
				screen.queryByRole("button", { name: "Actions for ci" }),
			).toBeNull(),
		);
		expect(
			screen.getByRole("button", { name: "Actions for other" }),
		).toBeInTheDocument();

		await user.click(screen.getByRole("radio", { name: "Revoked" }));
		const row = (await screen.findByText("ci")).closest("tr");
		if (!row) throw new Error("row missing");
		expect(within(row).getByText(/^Revoked /)).toBeInTheDocument();
		expect(
			within(row).queryByRole("button", { name: "Actions for ci" }),
		).toBeNull();
	});

	it("keeping the token leaves it active", async () => {
		const calls = mockTokens([makeToken("ci")]);
		const user = userEvent.setup();
		await openTokens();
		await user.click(
			await screen.findByRole("button", { name: "Actions for ci" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Revoke token" }),
		);
		await user.click(await screen.findByRole("button", { name: "Keep token" }));
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(calls.revoked).toEqual([]);
		await waitFor(() =>
			expect(
				screen.getByRole("button", { name: "Actions for ci" }),
			).toHaveFocus(),
		);
	});

	it("expiry states", async () => {
		mockTokens([
			makeToken("soon", { expires_at: Date.now() + 7 * DAY - 60_000 }),
			makeToken("later", { expires_at: Date.now() + 8 * DAY - 60_000 }),
			makeToken("forever", { expires_at: null }),
			makeToken("stale", { expires_at: Date.now() - DAY }),
		]);
		const user = userEvent.setup();
		await openTokens();
		await screen.findByText("soon");
		const row = (name: string) => {
			const tr = screen.getByText(name).closest("tr");
			if (!tr) throw new Error(`row ${name} missing`);
			return tr;
		};

		expect(within(row("soon")).getByText("In 7 days")).toHaveClass(
			"text-warning",
		);
		expect(within(row("later")).getByText("In 8 days")).not.toHaveClass(
			"text-warning",
		);
		expect(within(row("forever")).getByText("Never expires")).toBeVisible();
		expect(within(row("stale")).getByText("Expired")).toBeInTheDocument();
		expect(row("stale")).toHaveClass("text-muted-foreground");

		await user.click(
			within(row("stale")).getByRole("button", { name: "Actions for stale" }),
		);
		expect(
			await screen.findByRole("menuitem", { name: "Revoke token" }),
		).toBeInTheDocument();
	});

	it("empty states", async () => {
		mockTokens([]);
		const user = userEvent.setup();
		await openTokens();

		expect(await screen.findByText("No API tokens yet")).toBeInTheDocument();
		expect(
			screen.getByText(
				"Create a token with only the permissions your CLI or CI needs, then use it as LODESTORE_TOKEN.",
			),
		).toBeInTheDocument();
		expect(
			screen.getAllByRole("button", { name: "Create token" }),
		).toHaveLength(2);
		expect(screen.queryByRole("table")).toBeNull();

		await user.click(screen.getByRole("radio", { name: "Revoked" }));
		expect(
			await screen.findByText(
				"No revoked tokens. Revoked tokens appear here after you revoke one.",
			),
		).toBeInTheDocument();
		expect(screen.queryByText("No API tokens yet")).toBeNull();
	});

	it("shows five skeleton rows while loading", async () => {
		server.use(http.get("/api/v1/tokens", () => new Promise(() => {})));
		await openTokens();
		expect(await screen.findAllByTestId("skeleton-row")).toHaveLength(5);
	});

	it("load error retries", async () => {
		let attempts = 0;
		server.use(
			http.get("/api/v1/tokens", () => {
				attempts += 1;
				return attempts === 1
					? HttpResponse.json(
							{ code: "internal", message: "boom" },
							{ status: 500 },
						)
					: HttpResponse.json([makeToken("ci")]);
			}),
		);
		const user = userEvent.setup();
		await openTokens();

		expect(
			await screen.findByText(
				"Couldn't load API tokens. Check that the server is reachable, then retry.",
			),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Retry loading" }));
		expect(await screen.findByText("ci")).toBeInTheDocument();
		expect(screen.queryByText(/Couldn't load API tokens/)).toBeNull();
	});
});

describe("Settings navigation", () => {
	it("settings nav links repositories and api tokens", async () => {
		mockTokens([]);
		const user = userEvent.setup();
		const { router } = await openTokens();
		const nav = screen.getByRole("tablist", { name: "Settings sections" });
		expect(
			within(nav).getByRole("tab", { name: "API tokens" }),
		).toHaveAttribute("aria-selected", "true");
		expect(
			within(nav).getByRole("tab", { name: "Repositories" }),
		).toHaveAttribute("aria-selected", "false");

		await user.click(within(nav).getByRole("tab", { name: "Repositories" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/repositories"),
		);
		expect(
			await screen.findByRole("tab", { name: "Repositories" }),
		).toHaveAttribute("aria-selected", "true");
	});
});
