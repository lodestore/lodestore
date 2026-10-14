import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { renderApp, useSignedIn, useSignedOut } from "@/test/app";
import { server } from "@/test/server";

afterEach(() => {
	localStorage.clear();
	sessionStorage.clear();
});

describe("route guards", () => {
	it("sends a signed-out visitor to /login with the path to return to", async () => {
		useSignedOut();
		const { router } = renderApp("/hosts");
		await screen.findByRole("button", { name: "Sign in" });
		expect(router.state.location.pathname).toBe("/login");
		expect(router.state.location.href).toBe("/login?next=%2Fhosts");
	});

	it("sends a signed-out visitor to /login when the server has no administrator", async () => {
		server.use(
			http.get("/api/v1/auth/session", () =>
				HttpResponse.json(
					{ code: "setup_required", message: "setup required" },
					{ status: 409 },
				),
			),
			http.get("/api/v1/server/status", () =>
				HttpResponse.json({ setup_required: true }),
			),
		);
		const { router } = renderApp("/hosts");
		await screen.findByRole("button", { name: "Sign in" });
		expect(router.state.location.pathname).toBe("/login");
	});

	it("sends a signed-in user without repositories to /welcome", async () => {
		useSignedIn({ repositories: [] });
		const { router } = renderApp("/hosts");
		await screen.findByRole("heading", { level: 1 });
		expect(router.state.location.pathname).toBe("/welcome");
	});

	it("lets the repository form open without repositories", async () => {
		useSignedIn({ repositories: [] });
		const { router } = renderApp("/settings/repositories/new");
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/repositories/new"),
		);
	});

	it("asks for the server status once when /login opens", async () => {
		useSignedOut();
		let calls = 0;
		server.use(
			http.get("/api/v1/server/status", () => {
				calls += 1;
				return HttpResponse.json({ setup_required: false });
			}),
		);
		renderApp("/login");
		await screen.findByRole("button", { name: "Sign in" });
		expect(calls).toBe(1);
	});

	it("still checks the session when the server status cannot be read", async () => {
		useSignedIn();
		server.use(
			http.get(
				"/api/v1/server/status",
				() => new HttpResponse(null, { status: 500 }),
			),
		);
		const { router } = renderApp("/login");
		await waitFor(() => expect(router.state.location.pathname).toBe("/hosts"));
	});

	it("sends a signed-in user from /login to /hosts", async () => {
		useSignedIn();
		const { router } = renderApp("/login");
		await waitFor(() => expect(router.state.location.pathname).toBe("/hosts"));
	});

	it("keeps credentials and the CSRF token out of browser storage", async () => {
		useSignedOut();
		server.use(
			http.post(
				"/api/v1/auth/login",
				() => new HttpResponse(null, { status: 204 }),
			),
		);
		const { router } = renderApp("/login");
		const user = userEvent.setup();
		await user.type(await screen.findByLabelText("Username"), "admin");
		await user.type(screen.getByLabelText("Password"), "hunter2-secret");
		useSignedIn({ csrf: "csrf-secret-value" });
		await user.click(screen.getByRole("button", { name: "Sign in" }));
		await waitFor(() => expect(router.state.location.pathname).toBe("/hosts"));

		const stored = JSON.stringify({ ...localStorage, ...sessionStorage });
		expect(stored).not.toContain("hunter2-secret");
		expect(stored).not.toContain("csrf-secret-value");
		expect(localStorage.length + sessionStorage.length).toBe(0);
	});
});
