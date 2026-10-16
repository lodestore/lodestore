import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getListCatalogStatusQueryKey } from "@/api/gen/endpoints";
import type { CatalogStatus } from "@/api/gen/model";
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

		server.use(http.get("/api/v1/restore/hosts", () => HttpResponse.json([])));
		await router.navigate({ to: "/restore" });
		await screen.findByRole("heading", { level: 1, name: "Restore" });
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

const catalogStatus = (over: Partial<CatalogStatus> = {}): CatalogStatus => ({
	repository_id: "repo-1",
	repository_name: "main",
	phase: "idle",
	read: 0,
	total: null,
	added: 0,
	skipped: [],
	error: null,
	started_at: null,
	finished_at: null,
	last_sync_at: null,
	snapshots: 0,
	...over,
});

describe("rebuild banner", () => {
	it("shows progress and a link to the repository while rebuilding", async () => {
		useSignedIn();
		server.use(
			http.get("/api/v1/catalog", () =>
				HttpResponse.json([
					catalogStatus({ phase: "reading", read: 3, total: 10 }),
				]),
			),
		);
		renderApp("/overview");
		expect(
			await screen.findByText(
				"Rebuilding the catalog of main. 3 of 10 snapshots read.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("link", { name: "View progress" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1",
		);
		expect(
			screen.queryByRole("button", { name: /Dismiss/ }),
		).not.toBeInTheDocument();
		expect(screen.queryByRole("status")).toBeNull();
	});

	it("leaves out View progress on that repository's page", async () => {
		useSignedIn();
		server.use(
			http.get("/api/v1/catalog", () =>
				HttpResponse.json([
					catalogStatus({ phase: "reading", read: 3, total: 10 }),
				]),
			),
		);
		renderApp("/settings/repositories/repo-1");
		expect(
			await screen.findByText(
				"Rebuilding the catalog of main. 3 of 10 snapshots read.",
			),
		).toBeInTheDocument();
		expect(screen.queryByRole("link", { name: "View progress" })).toBeNull();
	});

	it("names the listing phase instead of a 0 of 0 counter", async () => {
		useSignedIn();
		server.use(
			http.get("/api/v1/catalog", () =>
				HttpResponse.json([catalogStatus({ phase: "listing" })]),
			),
		);
		renderApp("/overview");
		expect(
			await screen.findByText(
				"Rebuilding the catalog of main. Listing snapshots in storage.",
			),
		).toBeInTheDocument();
	});

	it("shows nothing when no catalog is rebuilding", async () => {
		useSignedIn();
		renderApp("/overview");
		await screen.findByRole("heading", { level: 1 });
		expect(
			screen.queryByText(/Rebuilding the catalog/),
		).not.toBeInTheDocument();
	});

	it("announces the end of a rebuild and offers the report when snapshots were skipped", async () => {
		const success = vi.spyOn(toast, "success").mockReturnValue(1);
		useSignedIn();
		let current = [catalogStatus({ phase: "reading", read: 1, total: 2 })];
		server.use(http.get("/api/v1/catalog", () => HttpResponse.json(current)));
		const { queryClient } = renderApp("/overview");
		await screen.findByText(/Rebuilding the catalog of main/);

		current = [
			catalogStatus({
				phase: "done",
				snapshots: 2,
				skipped: [{ key: "snapshots/aa", reason: "unreadable" }],
			}),
		];
		await queryClient.invalidateQueries({
			queryKey: getListCatalogStatusQueryKey(),
		});

		await waitFor(() =>
			expect(success).toHaveBeenCalledWith(
				"Catalog rebuilt: 2 snapshots.",
				expect.objectContaining({
					duration: 8000,
					action: expect.objectContaining({ label: "View report" }),
				}),
			),
		);
		expect(
			screen.queryByText(/Rebuilding the catalog/),
		).not.toBeInTheDocument();
	});

	it("uses a short toast without skipped snapshots", async () => {
		const success = vi.spyOn(toast, "success").mockReturnValue(1);
		useSignedIn();
		let current = [catalogStatus({ phase: "listing" })];
		server.use(http.get("/api/v1/catalog", () => HttpResponse.json(current)));
		const { queryClient } = renderApp("/overview");
		await screen.findByText(/Rebuilding the catalog of main/);

		current = [catalogStatus({ phase: "done", snapshots: 1 })];
		await queryClient.invalidateQueries({
			queryKey: getListCatalogStatusQueryKey(),
		});

		await waitFor(() =>
			expect(success).toHaveBeenCalledWith("Catalog rebuilt: 1 snapshot.", {
				duration: 4000,
			}),
		);
	});

	it("does not toast for a rebuild that was already finished on load", async () => {
		const success = vi.spyOn(toast, "success").mockReturnValue(1);
		useSignedIn();
		server.use(
			http.get("/api/v1/catalog", () =>
				HttpResponse.json([catalogStatus({ phase: "done", snapshots: 4 })]),
			),
		);
		renderApp("/overview");
		await screen.findByRole("heading", { level: 1 });
		expect(success).not.toHaveBeenCalled();
	});
});
