import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AddHostPanel, type AddHostStage } from "@/components/add-host-panel";
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
		command?: string;
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
					command:
						options.command ??
						`curl -fsSL http://localhost:8080/install.sh | sh -s -- --join ${SECRET}`,
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
			screen.getByText("Waiting for the host to connect…"),
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

	it("issues one token when Generate new token is double-clicked", async () => {
		mockServer({
			tokenStatus: {
				status: "expired",
				expires_at: realNow() - 1000,
				server_time: realNow(),
				host: null,
			},
		});
		let issuedTokens = 0;
		server.use(
			http.post("/api/v1/join-tokens", async () => {
				issuedTokens += 1;
				await new Promise((resolve) => setTimeout(resolve, 100));
				const serverTime = realNow();
				return HttpResponse.json(
					{
						id: "token-1",
						token: SECRET,
						command: `install --join ${SECRET}`,
						expires_at: serverTime + 3_600_000,
						server_time: serverTime,
					},
					{ status: 201 },
				);
			}),
		);
		renderPanel();
		await generate();
		const regenerate = await screen.findByRole("button", {
			name: "Generate new token",
		});
		await waitFor(() => expect(issuedTokens).toBe(1));

		await userEvent.setup().dblClick(regenerate);
		await waitFor(() => expect(issuedTokens).toBe(2));
		await new Promise((resolve) => setTimeout(resolve, 200));
		expect(issuedTokens).toBe(2);
	});

	it("HTTP command shows the root warning and the manual signature check", async () => {
		mockServer({
			command: `curl -fsSL http://localhost:8080/install.sh | sh -s -- --insecure --join ${SECRET}`,
		});
		renderPanel();
		await generate();

		expect(
			await screen.findByText(
				"Over plain HTTP anyone on the network can get root on this host.",
			),
		).toBeInTheDocument();
		await userEvent.setup().click(
			await screen.findByRole("button", {
				name: "Check the agent signature by hand",
			}),
		);
		expect(
			await screen.findByText(
				new RegExp(
					`minisign -V -P ${serverInfo.project_key.replace(/[+/]/g, "\\$&")} -m lodestore-agent`,
				),
			),
		).toBeInTheDocument();
		expect(
			screen.getByText(/\/install\/agent\/\$\(uname -m\)\.minisig/),
		).toBeInTheDocument();
	});

	it("HTTPS command shows no warning", async () => {
		mockServer({
			command: `curl --proto '=https' --tlsv1.2 -fsSL https://lode.example/install.sh | sh -s -- --join ${SECRET}`,
		});
		renderPanel();
		await generate();
		await screen.findByText(/--join lode1\./);

		expect(
			screen.queryByText(
				"Over plain HTTP anyone on the network can get root on this host.",
			),
		).toBeNull();
		expect(
			screen.queryByRole("button", {
				name: "Check the agent signature by hand",
			}),
		).toBeNull();
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

		expect(
			await screen.findByText(/This server has no agent binaries/),
		).toHaveTextContent("Add --from-github to the install command");
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

	it("reports the stages in order while the token is polled", async () => {
		mockServer();
		const answers = [
			{ status: "pending", host: null },
			{ status: "used", host: null },
			{
				status: "used",
				host: {
					id: "host-1",
					hostname: "db-1",
					version: "0.1.0",
					status: "online",
				},
			},
		];
		let polled = 0;
		server.use(
			http.get("/api/v1/join-tokens/token-1", () =>
				HttpResponse.json({
					expires_at: realNow() + 3_600_000,
					server_time: realNow(),
					...answers[Math.min(polled++, answers.length - 1)],
				}),
			),
		);
		const stages: AddHostStage[] = [];
		const onStage = (stage: AddHostStage) => stages.push(stage);
		renderPanel({ onStage });
		await generate();

		await waitFor(
			() => expect(stages).toEqual(["issued", "enrolling", "online"]),
			{
				timeout: 12_000,
			},
		);
	}, 15_000);
});
