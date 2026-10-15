import { cleanup, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

afterEach(() => {
	cleanup();
	localStorage.clear();
	sessionStorage.clear();
});

function mockServer() {
	useSignedIn({ repositories: [] });
	server.use(
		http.get("/api/v1/repositories", () => HttpResponse.json([])),
		http.get("/api/v1/agent-policy", () =>
			HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
		),
	);
}

describe("storage step", () => {
	it("marks only the selected backend card", async () => {
		mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");

		const s3 = await screen.findByRole("radio", { name: /S3-compatible/ });
		const local = screen.getByRole("radio", { name: /Local folder/ });
		const mark = (radio: HTMLElement) =>
			radio.closest("label")?.querySelector('[data-slot="backend-selected"]');
		expect(mark(s3)).not.toBeNull();
		expect(mark(local)).toBeNull();

		await user.click(local);
		expect(mark(local)).not.toBeNull();
		expect(mark(s3)).toBeNull();
	});
});

describe("access step", () => {
	it("explains write-only keys up front with a badge on the agent key", async () => {
		mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");

		await user.click(
			await screen.findByRole("button", { name: "Continue to access" }),
		);
		expect(await screen.findByText("Why write-only?")).toBeVisible();
		const keys = screen.getAllByRole("heading", { level: 3 });
		const titles = keys.map((h) => h.textContent);
		expect(titles).toEqual(
			expect.arrayContaining(["Bucket", "Server key", "Agent key"]),
		);
		const server = keys.find((h) => h.textContent === "Server key");
		const agent = keys.find((h) => h.textContent === "Agent key");
		expect(
			within(server?.parentElement as HTMLElement).queryByText("Write-only"),
		).toBeNull();
		expect(
			within(agent?.parentElement as HTMLElement).getByText("Write-only"),
		).toBeInTheDocument();
		expect(
			screen.getAllByRole("button", { name: "Check storage" }),
		).toHaveLength(1);
	});

	it("shows no key cards for a local folder", async () => {
		mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");

		await user.click(
			await screen.findByRole("radio", { name: /Local folder/ }),
		);
		await user.click(
			await screen.findByRole("button", { name: "Continue to access" }),
		);
		expect(await screen.findByLabelText("Path")).toBeInTheDocument();
		expect(screen.queryByText("Agent key")).toBeNull();
		expect(screen.queryByText("Why write-only?")).toBeNull();
		expect(
			screen.getAllByRole("button", { name: "Check storage" }),
		).toHaveLength(1);
	});
});
