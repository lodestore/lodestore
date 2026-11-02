import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, describe, expect, it, vi } from "vitest";
import { renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const PASSWORD = "correct horse battery";

afterEach(() => {
	vi.restoreAllMocks();
	localStorage.clear();
	sessionStorage.clear();
});

const localChecks = {
	existing_repository: false,
	blocking: false,
	agent_key_state: "unchecked",
	checks: [
		{ id: "write", status: "pass" },
		{ id: "list", status: "pass" },
		{ id: "conditional_write", status: "pass" },
		{
			id: "agent_delete",
			status: "neutral",
			error: "Not enforceable on a local folder",
		},
		{ id: "object_lock", status: "neutral", error: "not_applicable" },
	],
};

type Calls = { wizard: unknown[]; commit: unknown[]; check: unknown[] };

function mockServer() {
	const calls: Calls = { wizard: [], commit: [], check: [] };
	let created = false;
	const keys = ["lode-rk1-aaaa-bbbb-wxyz", "lode-rk1-cccc-dddd-qrst"];
	useSignedIn({ repositories: [] });
	server.use(
		http.get("/api/v1/repositories", () =>
			HttpResponse.json(created ? [repository] : []),
		),
		http.post("/api/v1/repositories/check", async ({ request }) => {
			calls.check.push(await request.json());
			return HttpResponse.json(localChecks);
		}),
		http.post("/api/v1/repositories/wizard", async ({ request }) => {
			calls.wizard.push(await request.json());
			return HttpResponse.json(
				{
					wizard_id: `w${calls.wizard.length}`,
					recovery_key: keys[(calls.wizard.length - 1) % keys.length],
					expires_at: Date.now() + 1_800_000,
				},
				{ status: 201 },
			);
		}),
		http.post("/api/v1/repositories/wizard/:id/commit", async ({ request }) => {
			calls.commit.push(await request.json());
			created = true;
			return HttpResponse.json(
				{ ...repository, name: "default" },
				{ status: 201 },
			);
		}),
	);
	return calls;
}

async function toAccess(user: ReturnType<typeof userEvent.setup>) {
	await user.click(await screen.findByRole("radio", { name: /Local folder/ }));
	await user.click(
		await screen.findByRole("button", { name: "Continue to access" }),
	);
	await user.type(await screen.findByLabelText("Path"), "/srv/lodestore");
}

async function checkStorage(user: ReturnType<typeof userEvent.setup>) {
	await user.click(screen.getByRole("button", { name: "Check storage" }));
	await user.click(
		await screen.findByRole("button", { name: "Continue to recovery key" }),
	);
}

describe("repository wizard with a local folder", () => {
	it("walks from storage to the first host", async () => {
		const calls = mockServer();
		const user = userEvent.setup();
		const { router } = renderApp("/welcome");

		expect(
			await screen.findByRole("heading", {
				name: "Set up your first repository",
			}),
		).toBeInTheDocument();
		expect(
			within(screen.getByRole("list", { name: "Progress" })).getAllByRole(
				"listitem",
			),
		).toHaveLength(4);
		expect(screen.getByRole("radio", { name: /S3-compatible/ })).toBeChecked();

		await toAccess(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));
		expect(
			await screen.findByText("The server key can write."),
		).toBeInTheDocument();
		expect(
			screen.getByText("The server key can list objects."),
		).toBeInTheDocument();
		expect(
			screen.getByText("The backend rejects overwriting existing objects."),
		).toBeInTheDocument();
		expect(
			screen.getByText("Not enforceable on a local folder."),
		).toBeInTheDocument();
		expect(calls.check).toEqual([
			{
				name: "default",
				location: { backend: "local", options: { path: "/srv/lodestore" } },
			},
		]);

		await user.click(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		);
		expect(
			await screen.findByText("lode-rk1-aaaa-bbbb-wxyz"),
		).toBeInTheDocument();
		const create = screen.getByRole("button", { name: "Create repository" });
		expect(create).toBeDisabled();

		const beforeUnload = new Event("beforeunload", { cancelable: true });
		window.dispatchEvent(beforeUnload);
		expect(beforeUnload.defaultPrevented).toBe(true);

		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		expect(create).toBeDisabled();
		await user.type(
			screen.getByLabelText("Type the last group of the key"),
			" WXYZ ",
		);
		expect(create).toBeEnabled();

		expect(JSON.stringify(sessionStorage)).not.toContain(PASSWORD);
		expect(JSON.stringify(sessionStorage)).not.toContain("wxyz");

		await user.click(create);
		expect(
			await screen.findByRole("heading", { name: "Add host" }),
		).toBeInTheDocument();
		expect(calls.commit).toEqual([
			{ password: PASSWORD, recovery_key_last_group: "WXYZ" },
		]);
		expect(
			screen.getByRole("button", { name: "Generate token" }),
		).toBeInTheDocument();

		const after = new Event("beforeunload", { cancelable: true });
		window.dispatchEvent(after);
		expect(after.defaultPrevented).toBe(false);
		expect(JSON.stringify(sessionStorage)).not.toContain(PASSWORD);

		await user.click(screen.getByRole("link", { name: "Skip adding a host" }));
		await waitFor(() => expect(router.state.location.pathname).toBe("/hosts"));
	});

	it("generates a new key after going back", async () => {
		const calls = mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await checkStorage(user);
		expect(
			await screen.findByText("lode-rk1-aaaa-bbbb-wxyz"),
		).toBeInTheDocument();

		await user.click(screen.getByRole("button", { name: "Previous step" }));
		await user.click(
			await screen.findByRole("button", { name: "Continue to recovery key" }),
		);
		expect(
			await screen.findByText("lode-rk1-cccc-dddd-qrst"),
		).toBeInTheDocument();
		expect(
			screen.queryByText("lode-rk1-aaaa-bbbb-wxyz"),
		).not.toBeInTheDocument();
		expect(calls.wizard).toHaveLength(2);
	});

	it("starts the key once under StrictMode", async () => {
		const calls = mockServer();
		const user = userEvent.setup();
		renderApp("/welcome", true);

		await toAccess(user);
		await checkStorage(user);
		expect(
			await screen.findByText("lode-rk1-aaaa-bbbb-wxyz"),
		).toBeInTheDocument();
		expect(calls.wizard).toHaveLength(1);
	});

	it("downloads the key under the repository name", async () => {
		mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");
		const createObjectURL = vi.fn(() => "blob:key");
		URL.createObjectURL = createObjectURL;
		URL.revokeObjectURL = vi.fn();
		const names: string[] = [];
		vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (
			this: HTMLAnchorElement,
		) {
			names.push(this.download);
		});

		await toAccess(user);
		await checkStorage(user);
		await user.click(
			await screen.findByRole("button", { name: "Download .txt" }),
		);
		expect(names).toEqual(["lodestore-recovery-key-default.txt"]);
		expect(createObjectURL).toHaveBeenCalledOnce();
	});

	it("starts at Storage after a reload", async () => {
		mockServer();
		renderApp("/welcome?step=key");
		expect(
			await screen.findByRole("heading", { name: "Choose storage" }),
		).toBeInTheDocument();
	});

	it("shows three steps when adding a repository from settings", async () => {
		mockServer();
		useSignedIn();
		renderApp("/settings/repositories/new");
		const list = await screen.findByRole("list", { name: "Progress" });
		expect(within(list).getAllByRole("listitem")).toHaveLength(3);
		expect(
			screen.queryByRole("heading", { name: "Set up your first repository" }),
		).not.toBeInTheDocument();
	});

	it("titles the settings wizard Add a repository", async () => {
		mockServer();
		useSignedIn();
		renderApp("/settings/repositories/new");
		expect(
			await screen.findByRole("heading", {
				level: 1,
				name: "Add a repository",
			}),
		).toBeInTheDocument();
		expect(
			screen.queryByText("Your progress is kept until you reload the page."),
		).not.toBeInTheDocument();
	});

	it("shows the progress footnote and no Exit setup on /welcome", async () => {
		mockServer();
		renderApp("/welcome");
		await screen.findByRole("heading", {
			name: "Set up your first repository",
		});
		expect(
			screen.getByText("Your progress is kept until you reload the page."),
		).toBeInTheDocument();
		expect(
			screen.queryByRole("button", { name: "Exit setup" }),
		).not.toBeInTheDocument();
		expect(screen.getByText("Step 1 of 4")).toBeInTheDocument();
		expect(
			screen.getByRole("progressbar", { name: "Progress" }),
		).toHaveAttribute("aria-valuenow", "1");
	});

	it("puts Exit setup in the title row of /settings/repositories/new", async () => {
		mockServer();
		useSignedIn();
		renderApp("/settings/repositories/new");
		const heading = await screen.findByRole("heading", {
			name: "Add a repository",
		});
		const header = heading.closest("header");
		expect(header).not.toBeNull();
		expect(
			within(header as HTMLElement).getByRole("button", {
				name: "Exit setup",
			}),
		).toBeInTheDocument();
	});

	it("leaves at once through Exit setup when nothing changed", async () => {
		mockServer();
		useSignedIn();
		const user = userEvent.setup();
		const { router } = renderApp("/settings/repositories/new");
		await user.click(await screen.findByRole("button", { name: "Exit setup" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/repositories"),
		);
	});

	it("asks before Exit setup discards changed fields", async () => {
		mockServer();
		useSignedIn();
		const user = userEvent.setup();
		const { router } = renderApp("/settings/repositories/new");
		await user.type(await screen.findByLabelText("Repository name"), "-two");
		await user.click(screen.getByRole("button", { name: "Exit setup" }));
		const dialog = await screen.findByRole("alertdialog", {
			name: "Leave without creating a repository?",
		});
		await user.click(
			within(dialog).getByRole("button", { name: "Keep editing" }),
		);
		expect(router.state.location.pathname).toBe("/settings/repositories/new");
		expect(screen.getByLabelText("Repository name")).toHaveValue("default-two");

		await user.click(screen.getByRole("button", { name: "Exit setup" }));
		await user.click(
			await screen.findByRole("button", { name: "Leave without creating" }),
		);
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/repositories"),
		);
		expect(sessionStorage.getItem("lodestore.wizard.draft")).toBeNull();
	});
});

const SERVER_SECRET = "SERVER-SECRET-VALUE";
const AGENT_SECRET = "AGENT-SECRET-VALUE";

type User = ReturnType<typeof userEvent.setup>;

function s3Result(
	statuses: Record<string, { status: string; error?: string }> = {},
	overrides: Record<string, unknown> = {},
) {
	const base: Record<string, { status: string; error?: string }> = {
		write: { status: "pass" },
		list: { status: "pass" },
		conditional_write: { status: "pass" },
		agent_delete: { status: "pass" },
		object_lock: { status: "neutral" },
		...statuses,
	};
	return {
		existing_repository: false,
		blocking: Object.values(base).some((c) => c.status === "fail"),
		agent_key_state: "verified",
		checks: Object.entries(base).map(([id, c]) => ({ id, ...c })),
		...overrides,
	};
}

function mockS3(result: ReturnType<typeof s3Result>) {
	const calls = mockServer();
	const buckets: string[] = [];
	server.use(
		http.post("/api/v1/repositories/check", async ({ request }) => {
			calls.check.push(await request.json());
			return HttpResponse.json(result);
		}),
		http.get("/api/v1/agent-policy", ({ request }) => {
			const bucket = new URL(request.url).searchParams.get("bucket") ?? "";
			buckets.push(bucket);
			return HttpResponse.json({
				iam_json: `{"Resource":"arn:aws:s3:::${bucket}/*"}`,
				minio_commands: `mc admin policy create local agent ${bucket}`,
			});
		}),
	);
	return { calls, buckets };
}

async function fillKeys(user: User) {
	await user.type(screen.getByLabelText("Server key: Access key ID"), "root");
	await user.type(
		screen.getByLabelText("Server key: Secret access key"),
		SERVER_SECRET,
	);
	await user.type(
		screen.getByLabelText("Agent key (write-only): Access key ID"),
		"agent",
	);
	await user.type(
		screen.getByLabelText("Agent key (write-only): Secret access key"),
		AGENT_SECRET,
	);
}

async function fillS3(user: User) {
	await user.click(
		await screen.findByRole("button", { name: "Continue to access" }),
	);
	await user.type(
		await screen.findByLabelText("Endpoint"),
		"http://minio:9000",
	);
	await user.type(screen.getByLabelText("Bucket"), "my-bucket");
	await fillKeys(user);
}

describe("repository wizard with S3", () => {
	it("keeps the policy in step with the bucket and sends both keys", async () => {
		const { calls, buckets } = mockS3(
			s3Result(
				{ agent_delete: { status: "warn" } },
				{ agent_key_state: "can_delete" },
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		expect(
			await screen.findByRole("radio", { name: /S3-compatible/ }),
		).toBeChecked();
		await user.click(
			screen.getByRole("button", { name: "Continue to access" }),
		);
		await waitFor(() => expect(buckets).toContain("YOUR-BUCKET"));
		expect(screen.getByText(/Two keys\./)).toBeInTheDocument();

		await user.type(screen.getByLabelText("Bucket"), "my-bucket");
		await waitFor(() => expect(buckets).toContain("my-bucket"));
		await user.click(screen.getByRole("tab", { name: "MinIO" }));
		expect(
			await screen.findByText(/mc admin policy create local agent my-bucket/),
		).toBeInTheDocument();

		expect(
			screen.getByRole("button", { name: "Check storage" }),
		).toBeDisabled();
		await user.type(screen.getByLabelText("Endpoint"), "http://minio:9000");
		await fillKeys(user);
		expect(
			screen.getByLabelText("Server key: Secret access key"),
		).toHaveAttribute("autocomplete", "off");
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText("The agent key can delete objects"),
		).toBeInTheDocument();
		expect(screen.getByText(/versioning or Object Lock/)).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeEnabled();
		expect(calls.check).toEqual([
			{
				name: "default",
				location: {
					backend: "s3",
					options: { bucket: "my-bucket", endpoint: "http://minio:9000" },
				},
				server_credentials: {
					access_key_id: "root",
					secret_access_key: SERVER_SECRET,
				},
				agent_credentials: {
					access_key_id: "agent",
					secret_access_key: AGENT_SECRET,
				},
			},
		]);

		await user.type(screen.getByLabelText("Bucket"), "2");
		await waitFor(() =>
			expect(document.querySelectorAll('[data-status="stale"]')).toHaveLength(
				5,
			),
		);
		expect(
			screen.queryByRole("button", { name: "Continue to recovery key" }),
		).not.toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Check storage" })).toBeEnabled();
	});

	it("blocks the key step on a failed write", async () => {
		mockS3(
			s3Result({
				write: { status: "fail", error: "AccessDenied" },
				list: {
					status: "neutral",
					error: "skipped because the write check failed",
				},
			}),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillS3(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText(
				"AccessDenied. Check the storage settings and the key permissions, then check again.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
		expect(
			screen.getByText(
				"Fix the failed checks to continue to the recovery key.",
			),
		).toBeInTheDocument();
	});

	it("explains an overwrite-capable key without advising Object Lock when it is on", async () => {
		mockS3(
			s3Result(
				{
					agent_delete: { status: "warn" },
					object_lock: { status: "pass" },
				},
				{ agent_key_state: "can_overwrite" },
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillS3(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText("The agent key can overwrite objects"),
		).toBeInTheDocument();
		expect(
			screen.getByText(/depends on its retention rules/),
		).toBeInTheDocument();
		expect(screen.queryByText(/versioning or Object Lock/)).toBeNull();
	});

	it("shows Object Lock notes as they are instead of calling them not enabled", async () => {
		mockS3(
			s3Result({
				object_lock: {
					status: "neutral",
					error: "skipped because the write check failed",
				},
			}),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillS3(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText("Skipped because the write check failed"),
		).toBeInTheDocument();
		expect(screen.queryByText(/^Not enabled/)).toBeNull();

		mockS3(
			s3Result({
				object_lock: {
					status: "warn",
					error: "could not check: access denied",
				},
			}),
		);
		await user.type(screen.getByLabelText("Bucket"), "2");
		await user.click(screen.getByRole("button", { name: "Check storage" }));
		expect(
			await screen.findByText("Could not check: access denied"),
		).toBeInTheDocument();
	});

	it("reports a failed request and lets the admin retry", async () => {
		mockS3(s3Result());
		server.use(
			http.post("/api/v1/repositories/check", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillS3(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));
		expect(
			await screen.findByText("Couldn't check the storage. Boom. Try again."),
		).toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Check storage" })).toBeEnabled();
	});

	it("connects an existing repository", async () => {
		const success = vi.spyOn(toast, "success");
		mockS3(s3Result({}, { existing_repository: true }));
		const bodies: unknown[] = [];
		server.use(
			http.post("/api/v1/repositories/connect", async ({ request }) => {
				bodies.push(await request.json());
				if (bodies.length === 1) {
					return HttpResponse.json(
						{ code: "validation", message: "wrong" },
						{ status: 422 },
					);
				}
				return HttpResponse.json(repository, { status: 201 });
			}),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillS3(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));
		expect(
			await screen.findByText("A Lodestore repository already exists here"),
		).toBeInTheDocument();
		await user.click(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		);

		expect(
			await screen.findByRole("tab", { name: "Password" }),
		).toBeInTheDocument();
		expect(
			screen.getByRole("tab", { name: "Recovery key" }),
		).toBeInTheDocument();
		expect(
			screen.queryByText("Save your recovery key"),
		).not.toBeInTheDocument();

		await user.type(
			screen.getByLabelText("Repository password"),
			"wrong password",
		);
		await user.click(
			screen.getByRole("button", { name: "Connect repository" }),
		);
		expect(
			await screen.findByText(
				"That password or recovery key does not open this repository. Check it and try again.",
			),
		).toBeInTheDocument();

		await user.click(
			screen.getByRole("button", { name: "Connect repository" }),
		);
		expect(
			await screen.findByText("Repository connected."),
		).toBeInTheDocument();
		expect(success).toHaveBeenCalledWith(
			"Repository connected.",
			expect.objectContaining({
				description:
					"Rebuilding the catalog from storage. Snapshots appear in Restore as they are read.",
				action: expect.objectContaining({ label: "Open Restore" }),
			}),
		);
		const step = (await screen.findByRole("heading", { name: "Add host" }))
			.parentElement as HTMLElement;
		expect(
			within(step).getByText(
				/^Rebuilding the catalog from storage\. Snapshots appear in Restore as they are read\./,
			),
		).toBeVisible();
		expect(
			within(step).getByRole("link", { name: "Open Restore" }),
		).toHaveAttribute("href", "/restore");
		expect(bodies[1]).toMatchObject({ password: "wrong password" });
	});

	it("offers only the backends agents can write to", async () => {
		mockS3(s3Result());
		renderApp("/welcome");

		await screen.findByRole("button", { name: "Continue to access" });
		expect(
			screen.getAllByRole("radio").map((radio) => radio.getAttribute("value")),
		).toEqual(["s3", "local"]);
		expect(screen.queryByRole("button", { name: /More backends/ })).toBeNull();
	});

	it("falls back to S3 when a stored draft names a hidden backend", async () => {
		sessionStorage.setItem(
			"lodestore.wizard.draft",
			JSON.stringify({ backend: "webdav", name: "stored" }),
		);
		mockS3(s3Result());
		renderApp("/welcome");

		await screen.findByRole("button", { name: "Continue to access" });
		expect(screen.getByRole("radio", { name: /S3-compatible/ })).toBeChecked();
	});

	it("restores non-secret fields after a reload and never stores secrets", async () => {
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");
		await fillS3(user);

		const stored = JSON.stringify(sessionStorage);
		expect(stored).toContain("my-bucket");
		expect(stored).toContain("http://minio:9000");
		expect(stored).not.toContain(SERVER_SECRET);
		expect(stored).not.toContain(AGENT_SECRET);
		expect(stored).not.toContain("root");

		cleanup();
		renderApp("/welcome");
		await user.click(
			await screen.findByRole("button", { name: "Continue to access" }),
		);
		expect(await screen.findByLabelText("Bucket")).toHaveValue("my-bucket");
		expect(screen.getByLabelText("Endpoint")).toHaveValue("http://minio:9000");
		expect(screen.getByLabelText("Server key: Secret access key")).toHaveValue(
			"",
		);
		expect(
			screen.getByLabelText("Agent key (write-only): Secret access key"),
		).toHaveValue("");
		expect(screen.getByLabelText("Server key: Access key ID")).toHaveValue("");
	});
});
