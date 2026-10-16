import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { repository, serverInfo } from "@/test/app";
import { server } from "@/test/server";
import { FirstHostStep } from "./first-host-step";
import { SuccessMoment } from "./success-moment";

const realNow = Date.now.bind(Date);

afterEach(() => {
	localStorage.clear();
	sessionStorage.clear();
});

const online = {
	id: "host-1",
	hostname: "web-01",
	version: "0.1.0",
	status: "online",
};

function mockServer(answers: { status: string; host: unknown }[]) {
	let polled = 0;
	server.use(
		http.get("/api/v1/server/info", () => HttpResponse.json(serverInfo)),
		http.get("/api/v1/repositories", () => HttpResponse.json([repository])),
		http.post("/api/v1/join-tokens", () => {
			const serverTime = realNow();
			return HttpResponse.json(
				{
					id: "token-1",
					token: "lode1.secret",
					command: "curl -fsSL http://localhost:8080/install.sh | sh",
					expires_at: serverTime + 3_600_000,
					server_time: serverTime,
				},
				{ status: 201 },
			);
		}),
		http.get("/api/v1/join-tokens/token-1", () =>
			HttpResponse.json({
				expires_at: realNow() + 3_600_000,
				server_time: realNow(),
				...answers[Math.min(polled++, answers.length - 1)],
			}),
		),
	);
}

function renderStep() {
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
		component: () => <FirstHostStep repositoryId="repo-1" />,
	});
	const router = createRouter({
		routeTree: root.addChildren([
			createRoute({ getParentRoute: () => root, path: "/hosts" }),
		]),
		history: createMemoryHistory({ initialEntries: ["/"] }),
	});
	render(
		<QueryClientProvider client={queryClient}>
			<RouterProvider router={router} />
		</QueryClientProvider>,
	);
}

function states() {
	return screen
		.getAllByRole("listitem")
		.filter((item) => item.dataset.state !== undefined)
		.map((item) => item.dataset.state);
}

describe("FirstHostStep", () => {
	it("lists the three timeline steps as pending before a token exists", async () => {
		mockServer([{ status: "pending", host: null }]);
		renderStep();

		await screen.findByRole("button", { name: "Generate token" });
		const items = screen.getAllByRole("listitem");
		expect(items.map((item) => item.textContent)).toEqual([
			"Run the command",
			"The agent enrolls",
			"The host appears here",
		]);
		expect(states()).toEqual(["pending", "pending", "pending"]);
		expect(
			screen.getByRole("link", { name: "Skip adding a host" }),
		).toBeVisible();
	});

	it("follows the token poll and ends in the success moment", async () => {
		mockServer([
			{ status: "pending", host: null },
			{ status: "used", host: null },
			{ status: "used", host: online },
		]);
		renderStep();
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Generate token" }),
		);

		await waitFor(() =>
			expect(states()).toEqual(["active", "pending", "pending"]),
		);
		expect(screen.getByText("Run the command").closest("li")).toHaveAttribute(
			"aria-current",
			"step",
		);

		await waitFor(
			() => expect(states()).toEqual(["done", "active", "pending"]),
			{
				timeout: 6000,
			},
		);

		const heading = await screen.findByText("web-01 is connected", undefined, {
			timeout: 6000,
		});
		const moment = heading.closest("[role=status]") as HTMLElement;
		expect(
			within(moment).getByText("Online, version 0.1.0."),
		).toBeInTheDocument();
		expect(
			within(moment).getByText("Next: create a backup policy"),
		).toBeInTheDocument();
		expect(screen.queryByText("What happens next")).toBeNull();
		expect(screen.getByRole("link", { name: "Open Hosts" })).toBeVisible();
		expect(
			screen.queryByRole("link", { name: "Skip adding a host" }),
		).toBeNull();
	}, 20_000);
});

describe("SuccessMoment", () => {
	async function renderMoment() {
		const root = createRootRoute({
			component: () => <SuccessMoment hostname="web-01" version="0.1.0" />,
		});
		const router = createRouter({
			routeTree: root.addChildren([
				createRoute({ getParentRoute: () => root, path: "/policies/new" }),
			]),
			history: createMemoryHistory({ initialEntries: ["/"] }),
		});
		render(<RouterProvider router={router} />);
		return screen.findByRole("status");
	}

	it("announces the host and draws the check after mounting", async () => {
		const region = await renderMoment();

		expect(within(region).getByText("web-01 is connected")).toHaveClass(
			"text-display",
		);
		const path = region.querySelector("path");
		await waitFor(() => expect(path).toHaveAttribute("stroke-dashoffset", "0"));
		expect(path).toHaveClass("duration-400");
	});

	it("links the next step to the new policy page", async () => {
		const region = await renderMoment();

		expect(
			within(region).getByRole("link", {
				name: "Next: create a backup policy",
			}),
		).toHaveAttribute("href", "/policies/new");
	});
});
