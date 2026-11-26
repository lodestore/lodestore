import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { LevelScale } from "@/components/guarantee-level";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

afterEach(() => {
	cleanup();
	localStorage.clear();
	sessionStorage.clear();
});

type User = ReturnType<typeof userEvent.setup>;

const WEAK = {
	existing_repository: false,
	blocking: false,
	agent_key_state: "can_delete",
	guarantee_level: "host_can_overwrite_or_delete",
	guarantee_notes: ["shared_credentials"],
	checks: [
		{ id: "write", status: "pass" },
		{ id: "list", status: "pass" },
		{ id: "conditional_write", status: "pass" },
		{ id: "agent_delete", status: "warn" },
		{ id: "object_lock", status: "neutral" },
	],
};

function mockProbe(result: Record<string, unknown> = WEAK) {
	const bodies: Record<string, unknown>[] = [];
	useSignedIn({ repositories: [] });
	server.use(
		http.get("/api/v1/repositories", () => HttpResponse.json([])),
		http.post("/api/v1/repositories/check", async ({ request }) => {
			bodies.push((await request.json()) as Record<string, unknown>);
			return HttpResponse.json(result);
		}),
	);
	return bodies;
}

async function toRights(user: User, tile: RegExp, account?: string) {
	await user.click(await screen.findByRole("radio", { name: tile }));
	await user.click(
		screen.getByRole("button", { name: "Continue to connection" }),
	);
	if (account) {
		await user.type(await screen.findByLabelText("Account ID"), account);
	} else {
		await user.type(await screen.findByLabelText("Endpoint"), "http://s3:9000");
	}
	await user.type(screen.getByLabelText("Bucket"), "my-bucket");
	await user.type(screen.getByLabelText("Server key: Access key ID"), "root");
	await user.type(
		screen.getByLabelText("Server key: Secret access key"),
		"SERVER-SECRET",
	);
	await user.click(
		screen.getByRole("button", { name: "Continue to agent rights" }),
	);
	await screen.findByRole("heading", {
		name: "Give agents a key that cannot delete",
	});
}

const CONSENT = { name: "I understand and want to continue" };

describe("agent rights step", () => {
	it("skips to the server key and asks for consent before continuing", async () => {
		const bodies = mockProbe();
		const user = userEvent.setup();
		renderApp("/welcome");
		await toRights(user, /MinIO/);

		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"agent",
		);
		await user.click(
			screen.getByRole("button", { name: "Skip, use the server key" }),
		);

		expect(
			await screen.findByText("A host could change or delete backups here"),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"Agents will use the server key. You can limit it later from the repository page.",
			),
		).toBeInTheDocument();
		expect(bodies).toHaveLength(1);
		expect(bodies[0].agent_credentials).toEqual({
			access_key_id: "root",
			secret_access_key: "SERVER-SECRET",
		});
		expect(screen.queryByLabelText(/Agent key/)).toBeNull();

		const next = screen.getByRole("button", {
			name: "Continue to recovery key",
		});
		expect(next).toBeDisabled();
		await user.click(screen.getByRole("checkbox", CONSENT));
		expect(next).toBeEnabled();

		await user.click(
			screen.getByRole("button", { name: "Use a limited key instead" }),
		);
		expect(
			await screen.findByLabelText("Agent key (write-only): Access key ID"),
		).toHaveValue("");
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
		expect(document.querySelectorAll('[data-status="stale"]')).toHaveLength(5);
	});

	it("tells the admin an R2 token cannot be limited and probes with the server key", async () => {
		const bodies = mockProbe();
		const user = userEvent.setup();
		renderApp("/welcome");
		await toRights(user, /Cloudflare R2/, "abc123");

		expect(screen.getByText("R2 tokens cannot be limited")).toBeVisible();
		expect(
			screen.getByText("agent-rights cloudflare-r2 my-bucket"),
		).toBeInTheDocument();
		expect(screen.queryByLabelText(/Agent key/)).toBeNull();
		expect(
			screen.queryByRole("button", { name: "Skip, use the server key" }),
		).toBeNull();

		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(bodies[0].agent_credentials).toEqual({
			access_key_id: "root",
			secret_access_key: "SERVER-SECRET",
		});
		expect(bodies[0].location).toEqual({
			backend: "s3",
			options: {
				bucket: "my-bucket",
				region: "auto",
				endpoint: "https://abc123.r2.cloudflarestorage.com",
				path_style: "false",
			},
		});
		await user.click(await screen.findByRole("checkbox", CONSENT));
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeEnabled();
	});

	it("marks tasks as done as the key is pasted and checked", async () => {
		mockProbe({ ...WEAK, guarantee_level: "create_only_enforced" });
		const user = userEvent.setup();
		renderApp("/welcome");
		await toRights(user, /MinIO/);

		const done = () => document.querySelectorAll("section[data-done]").length;
		expect(done()).toBe(0);
		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"agent",
		);
		await user.type(
			screen.getByLabelText("Agent key (write-only): Secret access key"),
			"AGENT-SECRET",
		);
		expect(done()).toBe(2);
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(done()).toBe(3));

		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"2",
		);
		await waitFor(() => expect(done()).toBe(2));
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
	});
});

describe("LevelScale", () => {
	it("leaves every segment empty and says Not measured for an unchecked level", () => {
		const { container } = render(<LevelScale level="unchecked" />);
		expect(screen.getByText("Not measured")).toBeInTheDocument();
		expect(container.querySelectorAll(".bg-warning, .bg-success")).toHaveLength(
			0,
		);
	});

	it("fills up to the measured level", () => {
		const { container } = render(
			<LevelScale level="overwrite_keeps_old_version" />,
		);
		expect(container.querySelectorAll(".bg-success")).toHaveLength(2);
		expect(screen.queryByText("Not measured")).toBeNull();
	});
});
