import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { apiError, renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const DAY = 86_400_000;

const s3Repository = {
	...repository,
	id: "repo-1",
	name: "main",
	agent_key_state: "can_delete",
	checked_at: Date.now() - 3 * DAY,
};

const azureRepository = {
	...repository,
	id: "repo-2",
	name: "archive",
	backend: "azblob",
	guarantee_level: "host_can_overwrite_or_delete",
	guarantee_accepted: "host_can_overwrite_or_delete",
	location_display: "azblob://container",
	agent_key_state: "unchecked",
	checked_at: null,
};

const detail = {
	...s3Repository,
	location: { backend: "s3", options: { bucket: "my-bucket", prefix: "lode" } },
	checks: [
		{ id: "write", status: "pass" },
		{ id: "list", status: "pass" },
		{ id: "conditional_write", status: "pass" },
		{ id: "agent_delete", status: "warn" },
		{ id: "object_lock", status: "neutral" },
	],
};

async function openAfterGuard(
	path: string,
	swap: () => void,
	first = "/overview",
) {
	const { router } = renderApp(first);
	await screen.findByRole("heading", {
		name: "Overview",
	});
	swap();
	await act(() => router.navigate({ to: path }));
}

describe("repositories list", () => {
	it("shows the backend, guarantee level and agent key state of every repository", async () => {
		useSignedIn({ repositories: [s3Repository, azureRepository] });
		renderApp("/settings/repositories");

		expect(await screen.findByText("main")).toBeInTheDocument();
		expect(
			screen.getAllByRole("columnheader").map((header) => header.textContent),
		).toEqual([
			"Name",
			"Backend",
			"Location",
			"Guarantee",
			"Hosts",
			"Agent key",
			"Last checked",
			"Cleanup",
			"Checks",
			"Actions",
		]);
		expect(screen.getByText("archive")).toBeInTheDocument();
		expect(
			screen.getByText("Host can overwrite or delete"),
		).toBeInTheDocument();
		expect(screen.getByText("Create-only enforced")).toBeInTheDocument();
		expect(screen.getByText("Can delete")).toBeInTheDocument();
		expect(screen.getByText("Not checked")).toBeInTheDocument();
		expect(screen.getByText("s3://bucket/prefix")).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Add repository" }),
		).toHaveAttribute("href", "/settings/repositories/new");
	});

	it("shows the level of every repository with how long ago it was checked", async () => {
		const row = (name: string, extra: Record<string, unknown>) => ({
			...repository,
			id: name,
			name,
			guarantee_checked_at: Date.now() - 3 * 3_600_000,
			...extra,
		});
		useSignedIn({
			repositories: [
				row("fresh", {}),
				row("old", { guarantee_checked_at: Date.now() - 3 * DAY - 1000 }),
				row("never", { guarantee_checked_at: null }),
				row("lower", { guarantee_level: "host_can_overwrite_or_delete" }),
				row("weak", {
					guarantee_level: "host_can_overwrite_or_delete",
					guarantee_accepted: "host_can_overwrite_or_delete",
				}),
				row("unmeasured", {
					guarantee_level: "unchecked",
					guarantee_accepted: "unchecked",
					guarantee_checked_at: null,
				}),
			],
		});
		renderApp("/settings/repositories");

		const cell = async (name: string) =>
			within(
				(await screen.findByRole("link", { name })).closest(
					"tr",
				) as HTMLElement,
			);
		const fresh = await cell("fresh");
		expect(fresh.getByText("Create-only enforced")).toBeInTheDocument();
		expect(fresh.getByText("3 hours ago")).toBeInTheDocument();
		expect(
			(await cell("old")).getByText("Not checked for 3 days"),
		).toBeInTheDocument();
		expect(
			(await cell("never")).getByText("Never checked"),
		).toBeInTheDocument();
		const lower = await cell("lower");
		expect(lower.getByText("Lower than accepted")).toBeInTheDocument();
		expect(lower.getByRole("link", { name: "See alert" })).toHaveAttribute(
			"href",
			"/settings/repositories/lower#guarantee",
		);
		expect(
			(await cell("weak")).getByText(/^Accepted · checked/),
		).toBeInTheDocument();
		expect(
			(await cell("unmeasured")).getByText("Unchecked"),
		).toBeInTheDocument();
	});

	it("shows the empty state when the list becomes empty", async () => {
		useSignedIn({ repositories: [s3Repository] });
		await openAfterGuard("/settings/repositories", () =>
			server.use(http.get("/api/v1/repositories", () => HttpResponse.json([]))),
		);

		expect(await screen.findByText("No repositories yet")).toBeInTheDocument();
		expect(
			screen.getAllByRole("link", { name: "Add repository" }),
		).toHaveLength(2);
	});

	it("shows the illustration and a way to add when no repository exists", async () => {
		useSignedIn({ repositories: [s3Repository] });
		await openAfterGuard("/settings/repositories", () =>
			server.use(http.get("/api/v1/repositories", () => HttpResponse.json([]))),
		);

		expect(await screen.findByText("No repositories yet")).toBeInTheDocument();
		expect(document.querySelectorAll("main img")).toHaveLength(1);
		expect(document.querySelector("main img")).toHaveAttribute(
			"src",
			expect.stringContaining("repos-empty"),
		);
		for (const link of screen.getAllByRole("link", {
			name: "Add repository",
		})) {
			expect(link).toHaveAttribute("href", "/settings/repositories/new");
		}
	});

	it("reveals the full location in a tooltip", async () => {
		const location = "s3://a-very-long-bucket-name/with/a/long/prefix/path";
		useSignedIn({
			repositories: [{ ...s3Repository, location_display: location }],
		});
		renderApp("/settings/repositories");
		const user = userEvent.setup();

		await user.tab();
		const trigger = await screen.findByText(location);
		await user.hover(trigger);
		expect(await screen.findByRole("tooltip")).toHaveTextContent(location);
	});

	it("keeps the previous rows and offers a retry when loading fails", async () => {
		useSignedIn({ repositories: [s3Repository] });
		await openAfterGuard("/settings/repositories", () =>
			server.use(
				http.get("/api/v1/repositories", () =>
					apiError(500, "internal", "boom"),
				),
			),
		);

		expect(
			await screen.findByText(/Couldn't load repositories/),
		).toBeInTheDocument();
		expect(screen.getByText("main")).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Retry loading" }),
		).toBeInTheDocument();
	});

	it("rechecks a repository from its row", async () => {
		useSignedIn({ repositories: [s3Repository] });
		let checked = 0;
		server.use(
			http.post("/api/v1/repositories/repo-1/check", () => {
				checked += 1;
				return HttpResponse.json({
					existing_repository: true,
					blocking: false,
					agent_key_state: "verified",
					checks: [],
				});
			}),
		);
		renderApp("/settings/repositories");
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Actions for main" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Check storage again" }),
		);
		await waitFor(() => expect(checked).toBe(1));
	});

	it("shows the cleanup state and links it to the cleanup card", async () => {
		useSignedIn({
			repositories: [
				{
					...s3Repository,
					cleanup: { state: "waiting", at: Date.now() + 2 * DAY },
				},
				{ ...azureRepository, cleanup: { state: "stopped", at: null } },
			],
		});
		renderApp("/settings/repositories");

		const waiting = await screen.findByRole("link", {
			name: /^Cleanup of main: Waiting/,
		});
		expect(waiting).toHaveTextContent("deletes in 2 days");
		expect(waiting).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#free-up-space",
		);
		expect(
			screen.getByRole("link", { name: /^Cleanup of archive: Stopped$/ }),
		).toBeInTheDocument();
	});

	it("shows the checks column", async () => {
		useSignedIn({
			repositories: [
				{
					...s3Repository,
					checks: { state: "failed", finished_at: Date.now() - 3_600_000 },
				},
				{
					...azureRepository,
					checks: { state: "never_run", finished_at: null },
				},
				{
					...azureRepository,
					id: "repo-3",
					name: "cold",
					checks: { state: "off", finished_at: null },
				},
			],
		});
		renderApp("/settings/repositories");

		const failed = await screen.findByRole("link", {
			name: /^Integrity checks of main: Failed/,
		});
		expect(failed).toHaveTextContent("1 hour ago");
		expect(failed).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1#checks",
		);
		expect(
			screen.getByRole("link", {
				name: /^Integrity checks of archive: Never run$/,
			}),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: /^Integrity checks of cold: Off$/ }),
		).toBeInTheDocument();
	});

	it("links the repository name to its page", async () => {
		useSignedIn({ repositories: [s3Repository] });
		renderApp("/settings/repositories");

		expect(await screen.findByRole("link", { name: "main" })).toHaveAttribute(
			"href",
			"/settings/repositories/repo-1",
		);
	});
});

function mockDetail(options: { local?: boolean } = {}) {
	let checkedAt = detail.checked_at;
	let rechecks = 0;
	const body = () =>
		options.local
			? {
					...detail,
					backend: "local",
					location: { backend: "local", options: { path: "/srv/repo" } },
				}
			: detail;
	server.use(
		http.get("/api/v1/repositories/repo-1", () =>
			HttpResponse.json({ ...body(), checked_at: checkedAt }),
		),
		http.post("/api/v1/repositories/repo-1/check", () => {
			rechecks += 1;
			checkedAt = Date.now();
			return HttpResponse.json({
				existing_repository: true,
				blocking: false,
				agent_key_state: "can_delete",
				checks: [],
			});
		}),
	);
	return { rechecks: () => rechecks };
}

describe("repository page", () => {
	it("shows Repository not found with a way back for an unknown id", async () => {
		useSignedIn();
		server.use(
			http.get("/api/v1/repositories/missing", () =>
				apiError(404, "not_found", "no"),
			),
		);
		renderApp("/settings/repositories/missing");

		expect(
			await screen.findByRole("heading", {
				level: 1,
				name: "Repository not found",
			}),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Back to Repositories" }),
		).toHaveAttribute("href", "/settings/repositories");
		expect(screen.queryByText(/Couldn't load this repository/)).toBeNull();
	});

	it("warns while the agent key can delete and lists the stored checks", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockDetail();
		server.use(
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");

		const alert = await screen.findByRole("alert");
		expect(
			within(alert).getByText("The agent key can delete objects"),
		).toBeInTheDocument();
		expect(
			screen.getAllByText("The agent key can delete objects"),
		).toHaveLength(1);
		expect(screen.getByText("Write objects")).toBeInTheDocument();
		expect(screen.getByText("Conditional write (If-None-Match)")).toBeVisible();
		expect(screen.getByText(/^Not enabled\. Optional/)).toBeInTheDocument();
		expect(screen.queryByText(/does not apply to this backend/)).toBeNull();
		expect(screen.getByText("3 days ago")).toBeInTheDocument();
	});

	it("shows why the server does not open the repository", async () => {
		useSignedIn({ repositories: [s3Repository] });
		server.use(
			http.get("/api/v1/repositories/repo-1", () =>
				HttpResponse.json({
					...detail,
					unavailable_reason:
						"corrupt config: differs from the configuration recorded when the repository was added",
				}),
			),
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");

		expect(
			await screen.findByText("The server does not open this repository"),
		).toBeInTheDocument();
		expect(
			screen.getByText(/differs from the configuration recorded/),
		).toBeInTheDocument();
	});

	it("shows no unavailable notice for a repository the server opens", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockDetail();
		server.use(
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");

		await screen.findByText("Write objects");
		expect(
			screen.queryByText("The server does not open this repository"),
		).toBeNull();
	});

	it("rechecks the storage and refreshes the check time", async () => {
		useSignedIn({ repositories: [s3Repository] });
		const mock = mockDetail();
		server.use(
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await screen.findByText("3 days ago");

		await user.click(
			screen.getByRole("button", { name: "Check storage again" }),
		);

		expect(await screen.findByText("just now")).toBeInTheDocument();
		expect(mock.rechecks()).toBe(1);
	});

	it("shows what a host can do in a card under the title and measures again on Check now", async () => {
		useSignedIn({ repositories: [s3Repository] });
		let checkedAt: number | null = null;
		let rechecks = 0;
		server.use(
			http.get("/api/v1/repositories/repo-1", () =>
				HttpResponse.json({
					...detail,
					guarantee_checked_at: checkedAt,
					guarantee_notes: ["agent_can_overwrite"],
				}),
			),
			http.post("/api/v1/repositories/repo-1/check", () => {
				rechecks += 1;
				checkedAt = Date.now();
				return HttpResponse.json({
					existing_repository: true,
					blocking: false,
					agent_key_state: "can_delete",
					checks: [],
				});
			}),
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();

		const title = await screen.findByRole("heading", {
			name: "What a host can do to backups",
		});
		const card = within(title.closest("#guarantee") as HTMLElement);
		expect(card.getAllByText("Create-only enforced")).toHaveLength(2);
		expect(card.getByText("Never checked")).toBeInTheDocument();
		expect(
			card.getByText(
				"A host can replace backups, and the storage keeps no earlier version.",
			),
		).toBeInTheDocument();

		await user.click(card.getByRole("button", { name: "Check now" }));

		expect(await card.findByText(/just now/)).toHaveTextContent("just now");
		expect(card.queryByText("Never checked")).toBeNull();
		expect(rechecks).toBe(1);
	});

	it("explains the backend when there are no notes, and shows an unmeasured level as such", async () => {
		useSignedIn({ repositories: [s3Repository] });
		server.use(
			http.get("/api/v1/repositories/repo-1", () =>
				HttpResponse.json({
					...detail,
					guarantee_level: "unchecked",
					guarantee_accepted: "unchecked",
				}),
			),
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
			),
		);
		renderApp("/settings/repositories/repo-1");

		const title = await screen.findByRole("heading", {
			name: "What a host can do to backups",
		});
		const card = within(title.closest("#guarantee") as HTMLElement);
		expect(card.getByText("Not measured")).toBeInTheDocument();
		expect(
			card.getByText("The agent key's rights decide what a host can do."),
		).toBeInTheDocument();
	});
});
