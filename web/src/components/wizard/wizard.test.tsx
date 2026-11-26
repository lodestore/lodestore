import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, describe, expect, it, vi } from "vitest";
import { renderApp, repository, useSignedIn } from "@/test/app";
import { server, storagePresets } from "@/test/server";

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
	guarantee_level: "create_only_enforced",
	guarantee_notes: [],
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
	await user.click(await screen.findByRole("radio", { name: /Local path/ }));
	await user.click(
		await screen.findByRole("button", { name: "Continue to connection" }),
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
		).toHaveLength(5);
		expect(
			screen.getByRole("button", { name: "Continue to connection" }),
		).toBeDisabled();

		await toAccess(user);
		expect(
			within(screen.getByRole("list", { name: "Progress" })).getAllByRole(
				"listitem",
			),
		).toHaveLength(4);
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
		expect(sessionStorage.getItem("lodestore.wizard.draft")).toBeNull();
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
			await screen.findByRole("heading", { name: "Connect storage" }),
		).toBeInTheDocument();
	});

	it("shows four steps when adding a repository from settings", async () => {
		mockServer();
		useSignedIn();
		const user = userEvent.setup();
		renderApp("/settings/repositories/new");
		const list = await screen.findByRole("list", { name: "Progress" });
		expect(within(list).getAllByRole("listitem")).toHaveLength(4);
		await user.click(await screen.findByRole("radio", { name: /Local path/ }));
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
		expect(screen.getByText("Step 1 of 5")).toBeInTheDocument();
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
		guarantee_level: "create_only_enforced",
		guarantee_notes: [],
		checks: Object.entries(base).map(([id, c]) => ({ id, ...c })),
		...overrides,
	};
}

function mockS3(result: ReturnType<typeof s3Result>) {
	const calls = mockServer();
	server.use(
		http.post("/api/v1/repositories/check", async ({ request }) => {
			calls.check.push(await request.json());
			return HttpResponse.json(result);
		}),
	);
	return calls;
}

async function fillServerKey(user: User) {
	await user.type(screen.getByLabelText("Server key: Access key ID"), "root");
	await user.type(
		screen.getByLabelText("Server key: Secret access key"),
		SERVER_SECRET,
	);
}

async function fillAgentKey(user: User) {
	await user.type(
		await screen.findByLabelText("Agent key (write-only): Access key ID"),
		"agent",
	);
	await user.type(
		screen.getByLabelText("Agent key (write-only): Secret access key"),
		AGENT_SECRET,
	);
}

async function chooseTile(user: User, name: RegExp) {
	await user.click(await screen.findByRole("radio", { name }));
	await user.click(
		screen.getByRole("button", { name: "Continue to connection" }),
	);
}

async function fillMinio(user: User) {
	await chooseTile(user, /MinIO/);
	await user.type(
		await screen.findByLabelText("Endpoint"),
		"http://minio:9000",
	);
	await user.type(screen.getByLabelText("Bucket"), "my-bucket");
	await fillServerKey(user);
	await user.click(
		screen.getByRole("button", { name: "Continue to agent rights" }),
	);
}

async function toRights(user: User) {
	await fillMinio(user);
	await fillAgentKey(user);
}

const CHECK = { name: "Check" };

describe("repository wizard with S3", () => {
	it("walks the MinIO preset from storage to the recovery key with a limited agent key", async () => {
		const { check } = mockS3(
			s3Result(
				{ agent_delete: { status: "warn" } },
				{
					agent_key_state: "can_delete",
					guarantee_level: "host_can_overwrite_or_delete",
					guarantee_notes: ["agent_can_delete"],
				},
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await fillMinio(user);
		expect(
			await screen.findByRole("heading", {
				name: "Give agents a key that cannot delete",
			}),
		).toBeInTheDocument();
		expect(
			await screen.findByText("agent-rights minio my-bucket"),
		).toBeInTheDocument();
		expect(
			screen.getByText("Run the check to see what this key can do."),
		).toBeInTheDocument();
		expect(screen.getByRole("button", CHECK)).toBeDisabled();

		await fillAgentKey(user);
		await user.click(screen.getByRole("button", CHECK));

		expect(
			await screen.findByText("The agent key can delete objects"),
		).toBeInTheDocument();
		expect(screen.getByText(/versioning or Object Lock/)).toBeInTheDocument();
		const next = screen.getByRole("button", {
			name: "Continue to recovery key",
		});
		expect(next).toBeDisabled();
		await user.click(
			screen.getByRole("checkbox", {
				name: "I understand and want to continue",
			}),
		);
		expect(next).toBeEnabled();
		expect(check).toEqual([
			{
				name: "default",
				location: {
					backend: "s3",
					options: {
						bucket: "my-bucket",
						endpoint: "http://minio:9000",
						path_style: "true",
					},
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

		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"2",
		);
		await waitFor(() =>
			expect(document.querySelectorAll('[data-status="stale"]')).toHaveLength(
				5,
			),
		);
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
		expect(screen.getByRole("button", { name: "Check again" })).toBeEnabled();
	});

	it("builds the Backblaze B2 endpoint from the region field and sends provider b2", async () => {
		const { check } = mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");

		await chooseTile(user, /Backblaze B2/);
		expect(screen.queryByLabelText("Endpoint")).toBeNull();
		await user.type(await screen.findByLabelText("Region"), "eu-central-003");
		expect(
			screen.getByText("https://s3.eu-central-003.backblazeb2.com"),
		).toBeInTheDocument();
		await user.type(screen.getByLabelText("Bucket"), "my-bucket");
		await fillServerKey(user);
		await user.click(
			screen.getByRole("button", { name: "Continue to agent rights" }),
		);
		await fillAgentKey(user);
		await user.click(screen.getByRole("button", CHECK));
		await screen.findByText("Result");
		await waitFor(() => expect(check).toHaveLength(1));

		expect(check).toEqual([
			expect.objectContaining({
				location: {
					backend: "s3",
					options: {
						bucket: "my-bucket",
						region: "eu-central-003",
						endpoint: "https://s3.eu-central-003.backblazeb2.com",
						path_style: "false",
						provider: "b2",
					},
				},
			}),
		]);
	});

	it("asks for each placeholder of a template endpoint", async () => {
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");

		await chooseTile(user, /Cloudflare R2/);
		await user.type(await screen.findByLabelText("Account ID"), "abc123");
		expect(screen.queryByLabelText("Region")).toBeNull();
		expect(
			screen.getByText("https://abc123.r2.cloudflarestorage.com"),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Continue to agent rights" }),
		).toBeDisabled();
	});

	it("marks only the selected tile", async () => {
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");

		const minio = await screen.findByRole("radio", { name: /MinIO/ });
		const local = screen.getByRole("radio", { name: /Local path/ });
		const mark = (radio: HTMLElement) =>
			radio.closest("label")?.querySelector('[data-slot="preset-selected"]');
		expect(mark(minio)).toBeNull();
		await user.click(minio);
		expect(mark(minio)).not.toBeNull();
		await user.click(local);
		expect(mark(local)).not.toBeNull();
		expect(mark(minio)).toBeNull();
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

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));

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

	it("hides the MinIO conditional-write hint once that check passed", async () => {
		const note = "mc commands for a user without delete.";
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");

		await toRights(user);
		expect(await screen.findByText(note)).toBeInTheDocument();
		await user.click(screen.getByRole("button", CHECK));
		await screen.findByText("The server key can write.");
		expect(screen.queryByText(note)).toBeNull();

		mockS3(
			s3Result({ conditional_write: { status: "warn", error: "unsupported" } }),
		);
		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"2",
		);
		await user.click(screen.getByRole("button", { name: "Check again" }));
		await screen.findByText(/Not supported\. Lodestore still works/);
		expect(screen.getByText(note)).toBeInTheDocument();
	});

	it("labels a disabled Object Lock as off, not as not applicable", async () => {
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));

		const row = (await screen.findByText(/^Not enabled\. Optional/)).closest(
			'[data-status="neutral"]',
		) as HTMLElement;
		expect(within(row).getByText("Off")).toBeInTheDocument();
		expect(within(row).queryByText("Not applicable")).toBeNull();
	});

	it("shows the host key choice as one joined control", async () => {
		mockServer();
		server.use(
			http.post("/api/v1/repositories/check", () =>
				HttpResponse.json({
					...s3Result(),
					host_key: { key: "ssh-ed25519 AAAA", fingerprint: "SHA256:abc" },
				}),
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");
		await user.click(await screen.findByRole("radio", { name: /SFTP/ }));
		await user.click(
			screen.getByRole("button", { name: "Continue to connection" }),
		);
		await user.type(await screen.findByLabelText("Address"), "nas:22");
		await user.click(screen.getByRole("button", { name: "Get host key" }));

		const group = await screen.findByRole("radiogroup", {
			name: "Is this your server?",
		});
		expect(group).toHaveAttribute("data-spacing", "0");
		await user.click(within(group).getByRole("radio", { name: "It matches" }));
		expect(
			within(group).getByRole("radio", { name: "It matches" }),
		).toBeChecked();
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

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));

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

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));

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
		await user.type(
			screen.getByLabelText("Agent key (write-only): Access key ID"),
			"2",
		);
		await user.click(screen.getByRole("button", { name: "Check again" }));
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

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));
		expect(
			await screen.findByText("Couldn't check the storage. Boom. Try again."),
		).toBeInTheDocument();
		expect(screen.getByRole("button", CHECK)).toBeEnabled();
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

		await toRights(user);
		await user.click(screen.getByRole("button", CHECK));
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

	it("offers every server preset, with Local path last", async () => {
		mockS3(s3Result());
		renderApp("/welcome");

		await screen.findByRole("radio", { name: /MinIO/ });
		expect(
			screen.getAllByRole("radio").map((radio) => radio.getAttribute("value")),
		).toEqual([
			"aws-s3",
			"minio",
			"wasabi",
			"backblaze-b2",
			"cloudflare-r2",
			"hetzner-object-storage",
			"hetzner-storage-box",
			"azure-blob",
			"google-cloud-storage",
			"sftp",
			"local",
		]);
		expect(
			screen.getByText("Cannot limit · use bucket lock"),
		).toBeInTheDocument();
		expect(
			screen.getByText("Bucket policy · not verified"),
		).toBeInTheDocument();
	});

	it("shows a retry when the providers do not load", async () => {
		mockS3(s3Result());
		let calls = 0;
		server.use(
			http.get("/api/v1/storage-presets", () => {
				calls += 1;
				return calls === 1
					? HttpResponse.json(
							{ code: "internal", message: "x" },
							{ status: 500 },
						)
					: HttpResponse.json(storagePresets);
			}),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		expect(
			await screen.findByText("Could not load providers"),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Retry" }));
		expect(await screen.findByRole("radio", { name: /MinIO/ })).toBeVisible();
	});

	it("ignores a stored preset of a hidden backend", async () => {
		sessionStorage.setItem(
			"lodestore.wizard.draft",
			JSON.stringify({
				name: "stored",
				preset: { id: "x", label: "X", backend: "webdav" },
			}),
		);
		mockS3(s3Result());
		renderApp("/welcome");

		await screen.findByRole("radio", { name: /MinIO/ });
		expect(screen.getByLabelText("Repository name")).toHaveValue("default");
	});

	it("restores non-secret fields after a reload and never stores secrets", async () => {
		mockS3(s3Result());
		const user = userEvent.setup();
		renderApp("/welcome");
		await toRights(user);

		const stored = JSON.stringify(sessionStorage);
		expect(stored).toContain("my-bucket");
		expect(stored).toContain("http://minio:9000");
		expect(stored).toContain("minio");
		expect(stored).not.toContain(SERVER_SECRET);
		expect(stored).not.toContain(AGENT_SECRET);
		expect(stored).not.toContain("root");
		expect(stored).not.toContain('"agent"');

		cleanup();
		renderApp("/welcome");
		await screen.findByRole("radio", { name: /MinIO/ });
		expect(screen.getByRole("radio", { name: /MinIO/ })).toBeChecked();
		await user.click(
			screen.getByRole("button", { name: "Continue to connection" }),
		);
		expect(await screen.findByLabelText("Bucket")).toHaveValue("my-bucket");
		expect(screen.getByLabelText("Endpoint")).toHaveValue("http://minio:9000");
		expect(screen.getByLabelText("Server key: Secret access key")).toHaveValue(
			"",
		);
		expect(screen.getByLabelText("Server key: Access key ID")).toHaveValue("");
	});
});

function mockLocal(overrides: Record<string, unknown>) {
	const calls = mockServer();
	server.use(
		http.post("/api/v1/repositories/check", async ({ request }) => {
			calls.check.push(await request.json());
			return HttpResponse.json({ ...localChecks, ...overrides });
		}),
	);
	return calls;
}

const CONSENT = { name: "I understand and want to continue" };

describe("guarantee consent", () => {
	it("asks for consent to a host that can overwrite and sends it with the draft", async () => {
		const calls = mockLocal({
			guarantee_level: "host_can_overwrite_or_delete",
			guarantee_notes: ["not_enforceable", "agent_can_overwrite"],
		});
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText("A host could change or delete backups here"),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"Every host that writes to this folder can change or delete its backups.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"A host can replace backups, and the storage keeps no earlier version.",
			),
		).toBeInTheDocument();
		const next = screen.getByRole("button", {
			name: "Continue to recovery key",
		});
		expect(next).toBeDisabled();

		await user.click(screen.getByRole("checkbox", CONSENT));
		expect(next).toBeEnabled();
		await user.click(next);
		await screen.findByText("lode-rk1-aaaa-bbbb-wxyz");
		expect(calls.wizard).toEqual([
			{
				name: "default",
				accept_weaker_guarantee: true,
				location: { backend: "local", options: { path: "/srv/lodestore" } },
			},
		]);
		expect(calls.check).toEqual([
			{
				name: "default",
				location: { backend: "local", options: { path: "/srv/lodestore" } },
			},
		]);
	});

	it("says an unmeasured level could not be measured and offers another check", async () => {
		const calls = mockLocal({ guarantee_level: "unchecked" });
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText("Lodestore could not measure what a host can do"),
		).toBeInTheDocument();
		const next = screen.getByRole("button", {
			name: "Continue to recovery key",
		});
		expect(next).toBeDisabled();
		await user.click(screen.getByRole("checkbox", CONSENT));
		expect(next).toBeEnabled();

		await user.click(screen.getByRole("button", { name: "Check again" }));
		await waitFor(() => expect(calls.check).toHaveLength(2));
		expect(await screen.findByRole("checkbox", CONSENT)).not.toBeChecked();
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
	});

	it("shows the consent block again when the server refuses a weaker level", async () => {
		mockLocal({});
		const weak = {
			...localChecks,
			guarantee_level: "host_can_overwrite_or_delete",
			guarantee_notes: ["not_enforceable"],
		};
		server.use(
			http.post("/api/v1/repositories/wizard", () =>
				HttpResponse.json(
					{ code: "weaker_guarantee", message: "weaker", check: weak },
					{ status: 409 },
				),
			),
		);
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await checkStorage(user);

		expect(
			await screen.findByText("A host could change or delete backups here"),
		).toBeInTheDocument();
		expect(screen.getByRole("checkbox", CONSENT)).not.toBeChecked();
		expect(screen.queryByText(/Couldn't/)).toBeNull();
	});

	it("shows an overwrite that keeps the old version as it is, without consent", async () => {
		mockLocal({ guarantee_level: "overwrite_keeps_old_version" });
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(await screen.findByText("Create-only")).toBeInTheDocument();
		expect(
			screen.getAllByText("Overwrite keeps old version").length,
		).toBeGreaterThan(1);
		expect(screen.queryByRole("checkbox", CONSENT)).toBeNull();
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeEnabled();
	});

	it("lists a note on a strong level without asking for consent", async () => {
		mockLocal({
			guarantee_level: "create_only_enforced",
			guarantee_notes: ["lifecycle_unverified"],
		});
		const user = userEvent.setup();
		renderApp("/welcome");

		await toAccess(user);
		await user.click(screen.getByRole("button", { name: "Check storage" }));

		expect(
			await screen.findByText(/lifecycle rules could not be read/),
		).toBeInTheDocument();
		expect(screen.queryByRole("checkbox", CONSENT)).toBeNull();
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeEnabled();
	});
});
