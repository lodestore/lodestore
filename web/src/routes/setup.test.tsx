import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { apiError, renderApp, useSignedIn, useSignedOut } from "@/test/app";
import { server } from "@/test/server";

const TOKEN = "setup-token-value";
const PASSWORD = "correct horse battery";

afterEach(() => {
	localStorage.clear();
	sessionStorage.clear();
});

function mockVerify(valid = true) {
	server.use(
		http.post("/api/v1/setup/verify", () =>
			valid
				? new HttpResponse(null, { status: 204 })
				: apiError(404, "not_found", "invalid setup link"),
		),
	);
}

async function fill(password: string, confirm: string) {
	const user = userEvent.setup();
	await user.type(await screen.findByLabelText("Password"), password);
	await user.type(screen.getByLabelText("Confirm password"), confirm);
	await user.click(
		screen.getByRole("button", { name: "Create administrator" }),
	);
}

describe("setup", () => {
	it("shows only the invalid-link alert when the token is rejected", async () => {
		useSignedOut(true);
		mockVerify(false);
		renderApp(`/setup?token=${TOKEN}`);

		expect(
			await screen.findByText(/This setup link is invalid or was already used/),
		).toBeInTheDocument();
		expect(screen.queryByLabelText("Password")).not.toBeInTheDocument();
	});

	it("rejects a password shorter than 12 characters without a request", async () => {
		useSignedOut(true);
		mockVerify();
		let posted = false;
		server.use(
			http.post("/api/v1/setup", () => {
				posted = true;
				return new HttpResponse(null, { status: 204 });
			}),
		);
		renderApp(`/setup?token=${TOKEN}`);
		await fill("elevenchars", "elevenchars");

		await waitFor(() =>
			expect(screen.getByLabelText("Password")).toHaveAttribute(
				"aria-invalid",
				"true",
			),
		);
		expect(screen.getByText("At least 12 characters.")).toBeInTheDocument();
		expect(posted).toBe(false);
	});

	it("flags a confirmation that does not match", async () => {
		useSignedOut(true);
		mockVerify();
		renderApp(`/setup?token=${TOKEN}`);
		await fill(PASSWORD, `${PASSWORD}x`);

		expect(
			await screen.findByText("Passwords do not match."),
		).toBeInTheDocument();
		expect(screen.getByLabelText("Confirm password")).toHaveAttribute(
			"aria-invalid",
			"true",
		);
	});

	it("signs in, opens /welcome and leaves no secret behind", async () => {
		useSignedOut(true);
		mockVerify();
		let body: unknown;
		server.use(
			http.post("/api/v1/setup", async ({ request }) => {
				body = await request.json();
				useSignedIn({ repositories: [] });
				return new HttpResponse(null, { status: 204 });
			}),
		);
		const { router } = renderApp(`/setup?token=${TOKEN}`);
		await fill(PASSWORD, PASSWORD);

		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/welcome"),
		);
		expect(body).toEqual({
			token: TOKEN,
			username: "admin",
			password: PASSWORD,
		});
		expect(router.state.location.href).not.toContain("token");
		expect(window.location.href).not.toContain(TOKEN);
		const stored = JSON.stringify({ ...localStorage, ...sessionStorage });
		expect(stored).not.toContain(TOKEN);
		expect(stored).not.toContain(PASSWORD);
		expect(stored).not.toContain("csrf-1");
	});

	it("does not verify the link again after creating the administrator", async () => {
		useSignedOut(true);
		let verifyCalls = 0;
		server.use(
			http.post("/api/v1/setup/verify", () => {
				verifyCalls += 1;
				return new HttpResponse(null, { status: 204 });
			}),
			http.post("/api/v1/setup", () => {
				useSignedIn({ repositories: [] });
				return new HttpResponse(null, { status: 204 });
			}),
		);
		const { router, queryClient } = renderApp(`/setup?token=${TOKEN}`);
		await fill(PASSWORD, PASSWORD);

		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/welcome"),
		);
		await new Promise((r) => setTimeout(r, 50));
		expect(verifyCalls).toBe(1);
		await waitFor(() =>
			expect(
				queryClient.getQueryCache().findAll({ queryKey: ["setup/verify"] }),
			).toHaveLength(0),
		);
	});

	it("does not ask for the session while setup is required", async () => {
		useSignedOut(true);
		mockVerify();
		let sessionCalls = 0;
		server.use(
			http.get("/api/v1/auth/session", () => {
				sessionCalls += 1;
				return apiError(409, "setup_required", "setup required");
			}),
		);
		renderApp(`/setup?token=${TOKEN}`);

		expect(await screen.findByLabelText("Password")).toBeInTheDocument();
		expect(sessionCalls).toBe(0);
	});

	it("sends the visitor to /login when an administrator already exists", async () => {
		useSignedOut();
		mockVerify();
		server.use(
			http.post("/api/v1/setup", () =>
				apiError(409, "conflict", "administrator already exists"),
			),
		);
		const { router } = renderApp(`/setup?token=${TOKEN}`);
		await fill(PASSWORD, PASSWORD);

		await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
	});

	it("sends a signed-in user to /hosts", async () => {
		useSignedIn();
		const { router } = renderApp(`/setup?token=${TOKEN}`);
		await waitFor(() => expect(router.state.location.pathname).toBe("/hosts"));
	});
});
