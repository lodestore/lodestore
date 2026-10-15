import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import { apiError, renderApp, useSignedIn, useSignedOut } from "@/test/app";
import { server } from "@/test/server";

async function fillAndSubmit(password = "wrong-password") {
	const user = userEvent.setup();
	await user.type(await screen.findByLabelText("Username"), "admin");
	await user.type(screen.getByLabelText("Password"), password);
	await user.click(screen.getByRole("button", { name: "Sign in" }));
}

afterEach(() => {
	sessionStorage.clear();
	vi.unstubAllGlobals();
});

describe("login", () => {
	it("renders the auth layout without the sidebar", async () => {
		useSignedOut();
		renderApp("/login");
		expect(
			await screen.findByRole("heading", {
				level: 1,
				name: "Sign in to Lodestore",
			}),
		).toBeInTheDocument();
		expect(screen.getByText("Backups outlive the server.")).toBeInTheDocument();
		expect(
			screen.queryByRole("navigation", { name: "Main navigation" }),
		).not.toBeInTheDocument();
	});

	it("warns about plain http before the password is typed", async () => {
		vi.stubGlobal("location", new URL("http://10.0.0.5:8080/login"));
		useSignedOut();
		renderApp("/login");
		expect(
			await screen.findByText("This connection is not encrypted"),
		).toBeInTheDocument();
	});

	it("shows the error, clears the password and focuses it on a wrong password", async () => {
		useSignedOut();
		server.use(
			http.post("/api/v1/auth/login", () =>
				apiError(401, "unauthorized", "invalid credentials"),
			),
		);
		renderApp("/login");
		await fillAndSubmit();

		expect(
			await screen.findByText("Incorrect username or password."),
		).toBeInTheDocument();
		const password = screen.getByLabelText("Password");
		await waitFor(() => expect(password).toHaveFocus());
		expect(password).toHaveValue("");
	});

	it("counts down when the server rate-limits the login", async () => {
		useSignedOut();
		server.use(
			http.post("/api/v1/auth/login", () =>
				HttpResponse.json(
					{ code: "rate_limited", message: "slow down" },
					{ status: 429, headers: { "Retry-After": "30" } },
				),
			),
		);
		renderApp("/login");
		await fillAndSubmit();

		expect(
			await screen.findByText(/Try again in 30 seconds\./),
		).toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Sign in" })).toBeDisabled();
	});

	it("tells the visitor when no administrator exists yet", async () => {
		useSignedOut(true);
		renderApp("/login");
		expect(
			await screen.findByText(/No administrator exists yet/),
		).toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Sign in" })).toBeEnabled();
	});

	it("explains an expired session", async () => {
		useSignedOut();
		renderApp("/login?next=%2Fhosts&expired=true");
		expect(
			await screen.findByText(
				"Your session expired. Sign in again to continue.",
			),
		).toBeInTheDocument();
	});

	it("returns to the requested page after signing in", async () => {
		useSignedOut();
		server.use(
			http.post(
				"/api/v1/auth/login",
				() => new HttpResponse(null, { status: 204 }),
			),
		);
		const { router } = renderApp("/login?next=%2Fpolicies");
		const user = userEvent.setup();
		await user.type(await screen.findByLabelText("Username"), "admin");
		await user.type(screen.getByLabelText("Password"), "correct-password");
		useSignedIn();
		await user.click(screen.getByRole("button", { name: "Sign in" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/policies"),
		);
	});

	it("shows an error when the session cannot be read after signing in", async () => {
		useSignedOut();
		server.use(
			http.post(
				"/api/v1/auth/login",
				() => new HttpResponse(null, { status: 204 }),
			),
			http.get("/api/v1/auth/session", () =>
				apiError(503, "unavailable", "try later"),
			),
		);
		const { router } = renderApp("/login");
		await fillAndSubmit("correct-password");

		expect(
			await screen.findByText(/Couldn't sign in\. try later/),
		).toBeInTheDocument();
		expect(router.state.location.pathname).toBe("/login");
		expect(screen.getByRole("button", { name: "Sign in" })).toBeEnabled();
	});

	it("ignores a next target that leaves the app", async () => {
		useSignedOut();
		const { router } = renderApp("/login?next=%2F%2Fevil.example");
		await screen.findByRole("button", { name: "Sign in" });
		expect(router.state.location.search.next).toBeUndefined();
	});
});
