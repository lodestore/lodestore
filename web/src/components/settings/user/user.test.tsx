import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

async function openUser() {
	useSignedIn();
	const app = renderApp("/settings/user");
	await screen.findByRole("heading", { name: "User", level: 1, hidden: true });
	return app;
}

function mockAccount(initial: string | null = null) {
	const calls = { saved: [] as unknown[] };
	let email = initial;
	server.use(
		http.get("/api/v1/account", () =>
			HttpResponse.json({ username: "admin", email }),
		),
		http.put("/api/v1/account", async ({ request }) => {
			const body = (await request.json()) as { email?: string | null };
			calls.saved.push(body);
			email = body.email ?? null;
			return HttpResponse.json({ username: "admin", email });
		}),
	);
	return calls;
}

const emailInput = () => screen.findByLabelText("Email");
const saveButton = () => screen.getByRole("button", { name: "Save email" });

describe("user: account", () => {
	it("shows the user tab", async () => {
		mockAccount();
		await openUser();
		const tabs = screen.getByRole("tablist", { name: "Settings sections" });
		const names = Array.from(tabs.querySelectorAll("a")).map(
			(a) => a.textContent,
		);
		expect(names).toEqual([
			"Repositories",
			"Notifications",
			"API tokens",
			"User",
		]);
		expect(
			screen.getByRole("tab", { name: "User", selected: true }),
		).toBeTruthy();
		expect(await screen.findByText("admin")).toBeTruthy();
	});

	it("saves the admin email", async () => {
		const calls = mockAccount();
		const user = userEvent.setup();
		await openUser();
		const input = await emailInput();
		expect((saveButton() as HTMLButtonElement).disabled).toBe(true);
		await user.type(input, "ops@example.com");
		await user.click(saveButton());
		expect(await screen.findByText("Email saved.")).toBeTruthy();
		expect(calls.saved).toEqual([{ email: "ops@example.com" }]);
		expect((saveButton() as HTMLButtonElement).disabled).toBe(true);
	});

	it("clears the admin email when emptied", async () => {
		const calls = mockAccount("ops@example.com");
		const user = userEvent.setup();
		await openUser();
		await user.clear(await emailInput());
		await user.click(saveButton());
		expect(await screen.findByText("Email saved.")).toBeTruthy();
		expect(calls.saved).toEqual([{ email: null }]);
	});

	it("rejects an invalid email", async () => {
		const calls = mockAccount();
		const user = userEvent.setup();
		await openUser();
		await user.type(await emailInput(), "nope");
		await user.click(saveButton());
		expect(
			await screen.findByText("Enter an address such as admin@example.com."),
		).toBeTruthy();
		expect(screen.getByLabelText("Email").getAttribute("aria-invalid")).toBe(
			"true",
		);
		expect(calls.saved).toEqual([]);
	});

	it("shows the email save error", async () => {
		mockAccount();
		server.use(
			http.put("/api/v1/account", () =>
				apiError(500, "internal", "database is locked"),
			),
		);
		const user = userEvent.setup();
		await openUser();
		await user.type(await emailInput(), "ops@example.com");
		await user.click(saveButton());
		const alert = await screen.findByRole("alert");
		expect(alert.textContent).toContain("Couldn't save the email.");
		expect(alert.textContent).toContain("Database is locked");
	});
});

function mockPassword(
	respond: () => Response = () => new HttpResponse(null, { status: 204 }),
) {
	const calls = { changed: [] as unknown[] };
	server.use(
		http.post("/api/v1/account/password", async ({ request }) => {
			calls.changed.push(await request.json());
			return respond();
		}),
	);
	return calls;
}

const currentField = () => screen.findByLabelText("Current password");
const newField = () => screen.getByLabelText("New password");
const confirmField = () => screen.getByLabelText("Confirm new password");
const changeButton = () =>
	screen.getByRole("button", { name: "Change password" }) as HTMLButtonElement;

async function fill(
	user: ReturnType<typeof userEvent.setup>,
	values: [string, string, string],
) {
	await user.type(await currentField(), values[0]);
	await user.type(newField(), values[1]);
	await user.type(confirmField(), values[2]);
}

describe("user: password", () => {
	it("enables change password when filled", async () => {
		mockAccount();
		const user = userEvent.setup();
		await openUser();
		await user.type(await currentField(), "old-password");
		expect(changeButton().disabled).toBe(true);
		expect(changeButton().dataset.variant).toBe("outline");
		await user.type(newField(), "a-new-password");
		await user.type(confirmField(), "a-new-password");
		expect(changeButton().disabled).toBe(false);
		expect(changeButton().dataset.variant).toBe("default");
		expect((await currentField()).getAttribute("autocomplete")).toBe(
			"current-password",
		);
		expect(newField().getAttribute("autocomplete")).toBe("new-password");
	});

	it("validates the new password", async () => {
		mockAccount();
		const calls = mockPassword();
		const user = userEvent.setup();
		await openUser();
		await fill(user, ["old-password", "short-pw", "short-pw"]);
		await user.click(changeButton());
		expect(await screen.findByText("Use at least 12 characters.")).toBeTruthy();
		expect(newField().getAttribute("aria-invalid")).toBe("true");

		await user.clear(newField());
		await user.type(newField(), "a-long-enough-password");
		await user.click(changeButton());
		expect(await screen.findByText("The passwords don't match.")).toBeTruthy();
		expect(calls.changed).toEqual([]);
	});

	it("shows a wrong current password on the field", async () => {
		mockAccount();
		mockPassword(() =>
			apiError(422, "wrong_password", "current password is wrong"),
		);
		const user = userEvent.setup();
		const { router } = await openUser();
		await fill(user, [
			"wrong",
			"a-long-enough-password",
			"a-long-enough-password",
		]);
		await user.click(changeButton());
		expect(await screen.findByText("Current password is wrong.")).toBeTruthy();
		expect((await currentField()).getAttribute("aria-invalid")).toBe("true");
		expect(router.state.location.pathname).toBe("/settings/user");
	});

	it("shows the rate limit", async () => {
		mockAccount();
		mockPassword(
			() =>
				HttpResponse.json(
					{ code: "rate_limited", message: "slow down" },
					{ status: 429, headers: { "Retry-After": "30" } },
				) as unknown as Response,
		);
		const user = userEvent.setup();
		await openUser();
		await fill(user, [
			"old-password",
			"a-long-enough-password",
			"a-long-enough-password",
		]);
		await user.click(changeButton());
		const alert = await screen.findByRole("alert");
		expect(alert.textContent).toBe(
			"Too many attempts. Try again in 30 seconds.",
		);
	});

	it("changes the password", async () => {
		mockAccount();
		const calls = mockPassword();
		const user = userEvent.setup();
		await openUser();
		await fill(user, [
			"old-password",
			"a-long-enough-password",
			"a-long-enough-password",
		]);
		await user.click(changeButton());
		expect(
			await screen.findByText(
				"Password changed. Other sessions were signed out.",
			),
		).toBeTruthy();
		expect(calls.changed).toEqual([
			{
				current_password: "old-password",
				new_password: "a-long-enough-password",
				revoke_tokens: false,
			},
		]);
		expect(((await currentField()) as HTMLInputElement).value).toBe("");
		expect((newField() as HTMLInputElement).value).toBe("");
		expect((confirmField() as HTMLInputElement).value).toBe("");
	});

	it("shows the busy server on the password form", async () => {
		mockAccount();
		mockPassword(() =>
			HttpResponse.json(
				{ code: "busy", message: "busy" },
				{ status: 503, headers: { "Retry-After": "2" } },
			),
		);
		const user = userEvent.setup();
		await openUser();
		await fill(user, [
			"old-password",
			"a-long-enough-password",
			"a-long-enough-password",
		]);
		await user.click(changeButton());
		expect((await screen.findByRole("alert")).textContent).toBe(
			"The server is busy. Try again in 2 seconds.",
		);
	});
});

function mockTokens(expiries: (number | null)[]) {
	server.use(
		http.get("/api/v1/tokens", () =>
			HttpResponse.json(
				expiries.map((expires_at, i) => ({
					id: `t${i}`,
					name: `token-${i}`,
					prefix: "lsk_a1b2c3",
					scopes: ["policies:read"],
					created_at: Date.now() - 1000,
					created_by: "admin",
					expires_at,
					last_used_at: null,
					revoked_at: null,
				})),
			),
		),
	);
}

describe("user: password revokes tokens", () => {
	const passwords: [string, string, string] = [
		"old-password",
		"a-long-enough-password",
		"a-long-enough-password",
	];

	it("offers to revoke the live tokens and sends the choice", async () => {
		mockAccount();
		mockTokens([null, Date.now() + 86_400_000, Date.now() - 1000]);
		const calls = mockPassword();
		const user = userEvent.setup();
		await openUser();
		const box = await screen.findByRole("checkbox", {
			name: "Revoke all API tokens (2)",
		});
		expect(box).toBeChecked();
		await fill(user, passwords);
		await user.click(changeButton());
		await waitFor(() => expect(calls.changed).toHaveLength(1));

		await user.click(box);
		expect(box).not.toBeChecked();
		await fill(user, passwords);
		await user.click(changeButton());
		await waitFor(() => expect(calls.changed).toHaveLength(2));
		expect(
			calls.changed.map((c) => (c as { revoke_tokens: boolean }).revoke_tokens),
		).toEqual([true, false]);
	});

	it("has no checkbox when no token is live", async () => {
		mockAccount();
		mockTokens([Date.now() - 1000]);
		const calls = mockPassword();
		const user = userEvent.setup();
		await openUser();
		await currentField();
		expect(screen.queryByRole("checkbox")).toBeNull();
		await fill(user, passwords);
		await user.click(changeButton());
		await waitFor(() => expect(calls.changed).toHaveLength(1));
		expect(calls.changed[0]).toMatchObject({ revoke_tokens: false });
	});
});
