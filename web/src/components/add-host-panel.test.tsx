import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddHostPanel } from "@/components/add-host-panel";
import { repository, serverInfo } from "@/test/app";
import { server } from "@/test/server";

const SECRET = "lode1.secret-token-value";
const realNow = Date.now.bind(Date);

afterEach(() => {
	vi.restoreAllMocks();
	localStorage.clear();
	sessionStorage.clear();
});

function mockServer(
	options: {
		repositories?: unknown[];
		agentArches?: string[];
		tokenStatus?: unknown;
		tokenLifetimeMs?: number;
	} = {},
) {
	server.use(
		http.get("/api/v1/server/info", () =>
			HttpResponse.json({
				...serverInfo,
				agent_arches: options.agentArches ?? ["x86_64"],
			}),
		),
		http.get("/api/v1/repositories", () =>
			HttpResponse.json(options.repositories ?? [repository]),
		),
		http.post("/api/v1/join-tokens", () => {
			const serverTime = realNow();
			return HttpResponse.json(
				{
					id: "token-1",
					token: SECRET,
					command: `curl -fsSL http://localhost:8080/install.sh | sh -s -- --join ${SECRET}`,
					expires_at: serverTime + (options.tokenLifetimeMs ?? 3_600_000),
					server_time: serverTime,
				},
				{ status: 201 },
			);
		}),
		http.get("/api/v1/join-tokens/token-1", () =>
			HttpResponse.json(
				options.tokenStatus ?? {
					status: "pending",
					expires_at: realNow() + 3_600_000,
					server_time: realNow(),
					host: null,
				},
			),
		),
	);
}

function renderPanel(props: Parameters<typeof AddHostPanel>[0] = {}) {
	window.matchMedia ??= ((query: string) => ({
		matches: false,
		media: query,
		addEventListener: () => {},
		removeEventListener: () => {},
	})) as unknown as typeof window.matchMedia;
	const queryClient = new QueryClient({
		defaultOptions: { queries: { retry: false } },
	});
	const root = createRootRoute({
		component: () => <AddHostPanel {...props} />,
	});
	const router = createRouter({
		routeTree: root.addChildren([
			createRoute({
				getParentRoute: () => root,
				path: "/settings/repositories/new",
			}),
		]),
		history: createMemoryHistory({ initialEntries: ["/"] }),
	});
	render(
		<QueryClientProvider client={queryClient}>
			<RouterProvider router={router} />
		</QueryClientProvider>,
	);
	return queryClient;
}

async function generate() {
	const user = userEvent.setup();
	await user.click(
		await screen.findByRole("button", { name: "Generate token" }),
	);
}

describe("AddHostPanel", () => {
	it("issues a token and shows the install command with a copy button", async () => {
		mockServer();
		renderPanel();
		await generate();

		expect(await screen.findByText(/--join lode1\./)).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Copy install command" }),
		).toBeEnabled();
		expect(
			screen.getByText("Waiting for the host to connect..."),
		).toBeInTheDocument();
	});

	it("counts down from the server clock, not the browser clock", async () => {
		mockServer();
		vi.spyOn(Date, "now").mockImplementation(() => realNow() + 600_000);
		renderPanel();
		await generate();

		expect(
			await screen.findByText(/Expires in 59:/, undefined, { timeout: 3000 }),
		).toBeInTheDocument();
		expect(
			screen.queryByText("The token expired. Generate a new one."),
		).toBeNull();
	});

	it("reports the host once it is online", async () => {
		const onConnected = vi.fn();
		mockServer({
			tokenStatus: {
				status: "used",
				expires_at: realNow() + 3_600_000,
				server_time: realNow(),
				host: {
					id: "host-1",
					hostname: "db-1",
					version: "0.1.0",
					status: "online",
				},
			},
		});
		renderPanel({ onConnected });
		await generate();

		expect(
			await screen.findByText("db-1 is online. Version 0.1.0."),
		).toBeInTheDocument();
		expect(onConnected).toHaveBeenCalledTimes(1);
		expect(onConnected).toHaveBeenCalledWith(
			expect.objectContaining({ hostname: "db-1" }),
		);
	});

	it("dims the command and offers a new token when the token expired", async () => {
		mockServer({
			tokenStatus: {
				status: "expired",
				expires_at: realNow() - 1000,
				server_time: realNow(),
				host: null,
			},
		});
		renderPanel();
		await generate();

		expect(
			await screen.findByText("The token expired. Generate a new one."),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Generate new token" }),
		).toBeEnabled();
		expect(
			screen.getByRole("button", { name: "Copy install command" }),
		).toBeDisabled();
	});

	it("asks for a repository first when there are none", async () => {
		mockServer({ repositories: [] });
		renderPanel();

		expect(
			await screen.findByText("Create a repository first"),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Add repository" }),
		).toHaveAttribute("href", "/settings/repositories/new");
	});

	it("warns when the server has no agent binaries", async () => {
		mockServer({ agentArches: [] });
		renderPanel();

		expect(await screen.findByText(/Add --from-github/)).toBeInTheDocument();
	});

	it("keeps the token out of storage, the URL and the query cache", async () => {
		mockServer();
		const queryClient = renderPanel();
		await generate();
		await screen.findByText(/--join lode1\./);

		const cached = JSON.stringify([
			...queryClient
				.getQueryCache()
				.getAll()
				.map((q) => q.state.data),
			...queryClient
				.getMutationCache()
				.getAll()
				.map((m) => m.state.data),
		]);
		expect(cached).not.toContain(SECRET);
		expect(
			JSON.stringify({ ...localStorage, ...sessionStorage }),
		).not.toContain(SECRET);
		expect(window.location.href).not.toContain(SECRET);
	});
});
