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
	experimental: true,
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
	it("shows the backend, experimental and agent key state of every repository", async () => {
		useSignedIn({ repositories: [s3Repository, azureRepository] });
		renderApp("/settings/repositories");

		expect(await screen.findByText("main")).toBeInTheDocument();
		expect(
			screen.getAllByRole("columnheader").map((header) => header.textContent),
		).toEqual([
			"Name",
			"Backend",
			"Location",
			"Hosts",
			"Agent key",
			"Last checked",
			"Cleanup",
			"Checks",
			"Actions",
		]);
		expect(screen.getByText("archive")).toBeInTheDocument();
		expect(screen.getByText("Experimental")).toBeInTheDocument();
		expect(screen.getByText("Can delete")).toBeInTheDocument();
		expect(screen.getByText("Not checked")).toBeInTheDocument();
		expect(screen.getByText("s3://bucket/prefix")).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Add repository" }),
		).toHaveAttribute("href", "/settings/repositories/new");
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

	it("shows the agent policy for the bucket of the repository", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockDetail();
		let requested: URLSearchParams | undefined;
		server.use(
			http.get("/api/v1/agent-policy", ({ request }) => {
				requested = new URL(request.url).searchParams;
				return HttpResponse.json({
					iam_json: "POLICY-FOR-my-bucket",
					minio_commands: "mc admin policy create",
				});
			}),
		);
		renderApp("/settings/repositories/repo-1");

		expect(await screen.findByText("POLICY-FOR-my-bucket")).toBeInTheDocument();
		expect(requested?.get("bucket")).toBe("my-bucket");
		expect(requested?.get("prefix")).toBe("lode");
	});
});

const host = (id: string, status: string, repositoryId = "repo-1") => ({
	id,
	hostname: id,
	status,
	version: "0.1.0",
	os: "linux",
	arch: "x86_64",
	labels: [],
	repository_id: repositoryId,
	cert_not_after: Date.now() + DAY,
	enrolled_at: Date.now() - DAY,
});

function mockKeyApi(
	verification: { status: string; error?: string },
	put?: { status: number; body: unknown },
	agentKeyState = "verified",
) {
	const calls = { verify: 0, put: [] as unknown[] };
	mockDetail();
	server.use(
		http.get("/api/v1/agent-policy", () =>
			HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
		),
		http.get("/api/v1/hosts", () =>
			HttpResponse.json([
				host("h1", "online"),
				host("h2", "offline"),
				host("h3", "online", "repo-2"),
			]),
		),
		http.post("/api/v1/repositories/repo-1/agent-key/verify", () => {
			calls.verify += 1;
			return HttpResponse.json({
				agent_key_state: agentKeyState,
				check: { id: "agent_delete", ...verification },
			});
		}),
		http.put("/api/v1/repositories/repo-1/agent-key", async ({ request }) => {
			calls.put.push(await request.json());
			return HttpResponse.json(
				put?.body ?? {
					agent_key_state: "verified",
					online_notified: 1,
					offline: 2,
				},
				{ status: put?.status ?? 200 },
			);
		}),
	);
	return calls;
}

async function fillKey(user: ReturnType<typeof userEvent.setup>) {
	await user.click(
		await screen.findByRole("button", { name: "Replace agent key" }),
	);
	await user.type(await screen.findByLabelText("Access key ID"), "AKIANEW");
	await user.type(screen.getByLabelText("Secret access key"), "s3cr3t");
	await user.click(
		screen.getByRole("button", { name: "Verify and replace key" }),
	);
}

describe("replace agent key", () => {
	it("keeps the current key when the new one fails verification", async () => {
		useSignedIn({ repositories: [s3Repository] });
		const calls = mockKeyApi({ status: "fail", error: "AccessDenied" });
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await fillKey(user);

		expect(
			await screen.findByText(
				"The new key could not be verified: AccessDenied. The current key is unchanged.",
			),
		).toBeInTheDocument();
		expect(calls.verify).toBe(1);
		expect(calls.put).toHaveLength(0);
		expect(screen.getByLabelText("Secret access key")).toHaveValue("");
	});

	it("names what the new key can do in the confirmation", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockKeyApi({ status: "warn" }, undefined, "can_overwrite");
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await fillKey(user);

		const confirm = await screen.findByRole("alertdialog");
		expect(
			within(confirm).getByText(/The agent key can overwrite objects\./),
		).toBeInTheDocument();
	});

	it("verifies, confirms and reports the online and offline agents", async () => {
		useSignedIn({ repositories: [s3Repository] });
		const calls = mockKeyApi({ status: "pass" });
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await fillKey(user);

		const confirm = await screen.findByRole("alertdialog");
		expect(
			within(confirm).getByText('Replace the agent key for "main"?'),
		).toBeInTheDocument();
		expect(
			within(confirm).getByText(/sends it to 1 online agent\./),
		).toBeInTheDocument();
		expect(calls.put).toHaveLength(0);

		await user.click(
			within(confirm).getByRole("button", { name: "Verify and replace key" }),
		);

		expect(
			await screen.findByText(
				"New key sent to 1 online agent. 2 offline agents will receive it when they reconnect.",
			),
		).toBeInTheDocument();
		expect(calls.put).toEqual([
			{ access_key_id: "AKIANEW", secret_access_key: "s3cr3t" },
		]);
		expect(screen.queryByLabelText("Secret access key")).toBeNull();
	});

	it("shows the server refusal and clears the secret", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockKeyApi(
			{ status: "pass" },
			{
				status: 422,
				body: {
					code: "validation",
					message:
						"The new key could not be verified: it cannot write. The current key is unchanged.",
				},
			},
		);
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await fillKey(user);
		const confirm = await screen.findByRole("alertdialog");
		await user.click(
			within(confirm).getByRole("button", { name: "Verify and replace key" }),
		);

		expect(
			await screen.findByText(
				/it cannot write\. The current key is unchanged\./,
			),
		).toBeInTheDocument();
		expect(screen.getByLabelText("Secret access key")).toHaveValue("");
	});

	it("is not offered for a local repository", async () => {
		useSignedIn({ repositories: [s3Repository] });
		mockDetail({ local: true });
		renderApp("/settings/repositories/repo-1");

		await screen.findByText("Storage checks");
		expect(
			screen.queryByRole("button", { name: "Replace agent key" }),
		).toBeNull();
	});
});
