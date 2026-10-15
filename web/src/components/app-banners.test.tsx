import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import { UnencryptedBanner } from "@/components/app-banners";
import { isInsecureOrigin } from "@/lib/insecure";
import { infoQuery, sessionQuery } from "@/lib/session";
import { apiError, renderApp, useSignedIn, useSignedOut } from "@/test/app";
import { server } from "@/test/server";

const UNLOCK = /The server's unlock key is stored next to the database/;

afterEach(() => {
	sessionStorage.clear();
	vi.unstubAllGlobals();
	vi.restoreAllMocks();
});

describe("isInsecureOrigin", () => {
	it("flags plain http on a non-local host only", () => {
		expect(isInsecureOrigin("http:", "10.0.0.5")).toBe(true);
		expect(isInsecureOrigin("http:", "localhost")).toBe(false);
		expect(isInsecureOrigin("http:", "127.0.0.1")).toBe(false);
		expect(isInsecureOrigin("http:", "[::1]")).toBe(false);
		expect(isInsecureOrigin("https:", "x")).toBe(false);
	});
});

describe("banners", () => {
	it("shows the unlock key banner and keeps it closed across routes", async () => {
		useSignedIn({ info: { unlock_key_source: "data_dir" } });
		const { router } = renderApp("/overview");
		expect(await screen.findByText(UNLOCK)).toBeInTheDocument();

		await userEvent.click(
			screen.getByRole("button", { name: "Dismiss unlock key warning" }),
		);
		expect(screen.queryByText(UNLOCK)).not.toBeInTheDocument();

		await router.navigate({ to: "/policies" });
		await screen.findByRole("heading", {
			name: "Decide what gets backed up, and when",
		});
		expect(screen.queryByText(UNLOCK)).not.toBeInTheDocument();
	});

	it("shows no unlock key banner when the key comes from the environment", async () => {
		useSignedIn({ info: { unlock_key_source: "env" } });
		renderApp("/overview");
		await screen.findByRole("heading", { level: 1 });
		expect(screen.queryByText(UNLOCK)).not.toBeInTheDocument();
	});

	it("warns about an unencrypted connection", async () => {
		vi.stubGlobal("location", new URL("http://10.0.0.5:8080/overview"));
		useSignedIn();
		renderApp("/overview");
		expect(
			await screen.findByText("This connection is not encrypted"),
		).toBeInTheDocument();
	});
});

describe("UnencryptedBanner", () => {
	it("shows on http and closes without asking the server", async () => {
		vi.stubGlobal("location", new URL("http://10.0.0.5:8080/login"));
		let asked = false;
		server.use(
			http.get("/api/v1/server/info", () => {
				asked = true;
				return new HttpResponse(null, { status: 401 });
			}),
		);
		render(<UnencryptedBanner />);
		expect(
			screen.getByText("This connection is not encrypted"),
		).toBeInTheDocument();
		await userEvent.click(
			screen.getByRole("button", {
				name: "Dismiss unencrypted connection warning",
			}),
		);
		expect(
			screen.queryByText("This connection is not encrypted"),
		).not.toBeInTheDocument();
		expect(asked).toBe(false);
	});
});

describe("UnencryptedBanner with blocked storage", () => {
	it("still renders and dismisses when sessionStorage throws", async () => {
		vi.stubGlobal("location", new URL("http://10.0.0.5:8080/login"));
		const blocked = () => {
			throw new DOMException("blocked", "SecurityError");
		};
		vi.spyOn(Storage.prototype, "getItem").mockImplementation(blocked);
		vi.spyOn(Storage.prototype, "setItem").mockImplementation(blocked);
		render(<UnencryptedBanner />);
		await userEvent.click(
			screen.getByRole("button", {
				name: "Dismiss unencrypted connection warning",
			}),
		);
		expect(
			screen.queryByText("This connection is not encrypted"),
		).not.toBeInTheDocument();
	});
});

describe("sidebar footer", () => {
	it("shows the server version and signs out", async () => {
		useSignedIn();
		let loggedOut = false;
		server.use(
			http.post("/api/v1/auth/logout", () => {
				loggedOut = true;
				useSignedOut();
				return new HttpResponse(null, { status: 204 });
			}),
		);
		const { router, queryClient } = renderApp("/overview");

		const user = userEvent.setup();
		await user.click(await screen.findByRole("button", { name: /admin/ }));
		expect(await screen.findByText("v0.1.0")).toBeInTheDocument();
		await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));

		await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
		expect(loggedOut).toBe(true);
		await waitFor(() =>
			expect(queryClient.getQueryData(infoQuery.queryKey)).toBeUndefined(),
		);
		expect(queryClient.getQueryData(sessionQuery.queryKey)).toBeUndefined();
	});

	it("signs out quietly when the session had already expired", async () => {
		useSignedIn();
		server.use(
			http.post("/api/v1/auth/logout", () => {
				useSignedOut();
				return apiError(401, "unauthorized", "authentication required");
			}),
		);
		const { router } = renderApp("/overview");

		const user = userEvent.setup();
		await user.click(await screen.findByRole("button", { name: /admin/ }));
		await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));

		await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
		expect(router.state.location.search.expired).toBeUndefined();
		expect(router.state.location.search.next).toBeUndefined();
		expect(screen.queryByText(/Couldn't sign out/)).toBeNull();
	});
});
