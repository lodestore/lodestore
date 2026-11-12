import { QueryClient } from "@tanstack/react-query";
import { createMemoryHistory, RouterProvider } from "@tanstack/react-router";
import { render, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { StrictMode } from "react";
import { afterEach, expect } from "vitest";
import { createAppRouter } from "@/router";
import { server } from "./server";

export const repository = {
	id: "repo-1",
	name: "main",
	backend: "s3",
	experimental: false,
	location_display: "s3://bucket/prefix",
	hosts_count: 0,
	agent_key_state: "verified",
	cleanup: { state: "off", at: null },
};

export const serverInfo = {
	version: "0.1.0",
	unlock_key_source: "env",
	public_url: "http://localhost:8080",
	grpc_public_address: "backup.example.com:8443",
	agent_arches: ["x86_64"],
	project_key: "RWR4L6XuZIijjGDS/pggnqvsOKeXTnAUspdDGzyF983eIpcnS+mKEREI",
	demo_mode: false,
};

export function apiError(status: number, code: string, message: string) {
	return HttpResponse.json({ code, message }, { status });
}

export function useSignedIn(
	options: {
		repositories?: unknown[];
		info?: Partial<typeof serverInfo>;
		csrf?: string;
	} = {},
) {
	server.use(
		http.get("/api/v1/auth/session", () =>
			HttpResponse.json({
				username: "admin",
				csrf_token: options.csrf ?? "csrf-1",
			}),
		),
		http.get("/api/v1/server/status", () =>
			HttpResponse.json({ setup_required: false, demo_mode: false }),
		),
		http.get("/api/v1/server/info", () =>
			HttpResponse.json({ ...serverInfo, ...options.info }),
		),
		http.get("/api/v1/repositories", () =>
			HttpResponse.json(options.repositories ?? [repository]),
		),
	);
}

export function useSignedOut(setupRequired = false) {
	server.use(
		http.get("/api/v1/auth/session", () =>
			apiError(401, "unauthorized", "authentication required"),
		),
		http.get("/api/v1/server/status", () =>
			HttpResponse.json({ setup_required: setupRequired, demo_mode: false }),
		),
	);
}

const mounted: [ReturnType<typeof createAppRouter>, HTMLElement][] = [];

// Runs before cleanup() in setup.ts (after-hooks run in reverse order): a load still running at unmount resumes after jsdom teardown.
afterEach(async () => {
	await waitFor(() => {
		for (const [router, container] of mounted) {
			if (container.isConnected) expect(router.state.status).toBe("idle");
		}
	});
	mounted.length = 0;
});

export function renderApp(path: string, strict = false) {
	window.matchMedia ??= ((query: string) => ({
		matches: false,
		media: query,
		addEventListener: () => {},
		removeEventListener: () => {},
	})) as unknown as typeof window.matchMedia;
	const queryClient = new QueryClient({
		defaultOptions: { queries: { retry: false } },
	});
	const router = createAppRouter(
		createMemoryHistory({ initialEntries: [path] }),
		queryClient,
	);
	const tree = <RouterProvider router={router} />;
	const { container } = render(strict ? <StrictMode>{tree}</StrictMode> : tree);
	mounted.push([router, container]);
	return { router, queryClient };
}
