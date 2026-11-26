import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, describe, expect, it } from "vitest";
import { apiError, renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const s3Repository = {
	...repository,
	id: "repo-1",
	name: "main",
	agent_key_state: "can_delete",
	guarantee_level: "host_can_overwrite_or_delete",
};

const accessOf = (patch: Record<string, unknown> = {}) => ({
	after_revoke: "until_rotation",
	status: "no_revoked_hosts",
	rotate_via: "agent-key",
	access_ends_by: null,
	hosts_need_agent_update: 0,
	revoked_hosts_before_rotation: 0,
	revoked_hosts_since_rotation: 0,
	...patch,
});

const detailOf = (backend: string, options: Record<string, string>) => ({
	...s3Repository,
	backend,
	location: { backend, options },
	checks: [],
});

type Mock = {
	puts: { path: string; body: unknown }[];
	policy: URLSearchParams[];
	stepUps: number;
	hostKey: { checks: number; puts: unknown[] };
};

function mockRepository(
	detail: ReturnType<typeof detailOf>,
	options: {
		access?: Record<string, unknown>;
		reply?: () => Response;
		replyBody?: Record<string, unknown>;
		rights?: { mechanism: string; text: string };
		offer?: Record<string, unknown>;
	} = {},
): Mock {
	const mock: Mock = {
		puts: [],
		policy: [],
		stepUps: 0,
		hostKey: { checks: 0, puts: [] },
	};
	const put =
		(path: string) =>
		async ({ request }: { request: Request }) => {
			mock.puts.push({ path, body: await request.json() });
			return (
				options.reply?.() ??
				HttpResponse.json(
					options.replyBody ?? {
						agent_key_state: "verified",
						online_notified: 1,
						offline: 2,
					},
				)
			);
		};
	useSignedIn({ repositories: [s3Repository] });
	server.use(
		http.get("/api/v1/repositories/repo-1", () => HttpResponse.json(detail)),
		http.get("/api/v1/repositories/repo-1/storage-access", () =>
			HttpResponse.json(accessOf(options.access)),
		),
		http.get("/api/v1/agent-policy", ({ request }) => {
			mock.policy.push(new URL(request.url).searchParams);
			return HttpResponse.json({
				iam_json: "{}",
				minio_commands: "mc",
				rights: {
					mechanism: options.rights?.mechanism ?? "policy",
					note: "",
					preset: new URL(request.url).searchParams.get("preset"),
					server_text: null,
					text: options.rights?.text ?? "COMMANDS",
					verified: true,
				},
			});
		}),
		http.put("/api/v1/repositories/repo-1/agent-key", put("agent-key")),
		http.put("/api/v1/repositories/repo-1/agent-options", put("agent-options")),
		http.post("/api/v1/repositories/repo-1/host-key/check", () => {
			mock.hostKey.checks += 1;
			return HttpResponse.json(options.offer);
		}),
		http.put("/api/v1/repositories/repo-1/host-key", async ({ request }) => {
			mock.hostKey.puts.push(await request.json());
			return HttpResponse.json({
				fingerprint: "SHA256:new",
				needs_update: 0,
				offline: 0,
				online_notified: 1,
			});
		}),
		http.post("/api/v1/auth/step-up", () => {
			mock.stepUps += 1;
			return new HttpResponse(null, { status: 204 });
		}),
	);
	return mock;
}

const s3Detail = detailOf("s3", { bucket: "my-bucket", prefix: "lode" });

async function openSheet(user: ReturnType<typeof userEvent.setup>) {
	renderApp("/settings/repositories/repo-1");
	const [trigger] = await screen.findAllByRole("button", {
		name: "Limit agent rights",
	});
	await user.click(trigger);
	return {
		trigger,
		sheet: await screen.findByRole("dialog", { name: "Limit agent rights" }),
	};
}

async function fillS3Key(
	sheet: HTMLElement,
	user: ReturnType<typeof userEvent.setup>,
) {
	await user.type(
		await within(sheet).findByLabelText(/Access key ID/),
		"AKIANEW",
	);
	await user.type(within(sheet).getByLabelText(/Secret access key/), "s3cr3t");
}

const submit = (sheet: HTMLElement) =>
	within(sheet).getByRole("button", { name: "Check and switch agents" });

async function confirmPassword(user: ReturnType<typeof userEvent.setup>) {
	const dialog = await screen.findByRole("dialog", {
		name: "Confirm your password",
	});
	await user.type(within(dialog).getByLabelText("Password"), "hunter2hunter2");
	await user.click(within(dialog).getByRole("button", { name: "Confirm" }));
}

describe("limit agent rights on the repository page", () => {
	afterEach(() => {
		toast.dismiss();
	});

	it("done names the provider: asks for the password, sends the key, closes and reports the agents", async () => {
		const mock = mockRepository(s3Detail);
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);

		expect(await within(sheet).findByText("COMMANDS")).toBeInTheDocument();
		expect(mock.policy[0]?.get("preset")).toBe("aws-s3");
		expect(mock.policy[0]?.get("bucket")).toBe("my-bucket");
		expect(mock.policy[0]?.get("prefix")).toBe("lode");
		expect(submit(sheet)).toBeDisabled();

		await fillS3Key(sheet, user);
		await user.click(submit(sheet));
		await confirmPassword(user);

		expect(
			await screen.findByText(
				"Agents switched to the new key. Disable the old one at Amazon S3.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"New key sent to 1 online agent. 2 offline agents will receive it when they reconnect.",
			),
		).toBeInTheDocument();
		expect(mock.puts).toEqual([
			{
				path: "agent-key",
				body: { access_key_id: "AKIANEW", secret_access_key: "s3cr3t" },
			},
		]);
		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Limit agent rights" }),
			).toBeNull(),
		);
	});

	it("doneShared for a server key: no advice to disable the old key", async () => {
		mockRepository(s3Detail, {
			access: {
				after_revoke: "server_key_shared",
				rotate_via: null,
				status: "revoked_hosts_hold_access",
			},
		});
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await fillS3Key(sheet, user);
		await user.click(submit(sheet));
		await confirmPassword(user);

		expect(
			await screen.findByText(
				"Agents switched to the new key. The server keeps its own key.",
			),
		).toBeInTheDocument();
		expect(screen.queryByText(/Disable the old one/)).toBeNull();
	});

	it("cancel step-up -> no PUT: nothing is sent, no toast, the sheet stays open", async () => {
		const mock = mockRepository(s3Detail);
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await fillS3Key(sheet, user);
		await user.click(submit(sheet));

		const dialog = await screen.findByRole("dialog", {
			name: "Confirm your password",
		});
		await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(mock.puts).toHaveLength(0);
		expect(screen.queryByText(/Agents switched/)).toBeNull();
		expect(screen.getByRole("dialog", { name: "Limit agent rights" })).toBe(
			sheet,
		);
		expect(within(sheet).queryByRole("alert")).toBeNull();
		expect(submit(sheet)).toBeEnabled();
	});

	it("422 keeps the sheet open with the server's reason", async () => {
		mockRepository(s3Detail, {
			reply: () =>
				apiError(
					422,
					"validation",
					"The new key could not be verified: it cannot write. The current key is unchanged.",
				),
		});
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await fillS3Key(sheet, user);
		await user.click(submit(sheet));
		await confirmPassword(user);

		expect(
			await within(sheet).findByText(/it cannot write\. The current key/),
		).toBeInTheDocument();
		expect(screen.queryByText(/Agents switched/)).toBeNull();
		expect(screen.getByLabelText(/Access key ID/)).toHaveValue("AKIANEW");
	});

	it("Escape closes the sheet and returns focus to the button that opened it", async () => {
		mockRepository(s3Detail);
		const user = userEvent.setup();
		const { trigger } = await openSheet(user);

		await user.keyboard("{Escape}");

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Limit agent rights" }),
			).toBeNull(),
		);
		expect(trigger).toHaveFocus();
	});

	it("is not offered for a local repository", async () => {
		mockRepository(detailOf("local", { path: "/srv/repo" }));
		renderApp("/settings/repositories/repo-1");

		await screen.findByText("What a host can do to backups");
		expect(
			screen.queryByRole("button", { name: "Limit agent rights" }),
		).toBeNull();
	});
});

const sftpDetail = detailOf("sftp", {
	endpoint: "ssh://backup.example.com:22",
	user: "srv",
	root: "/backups",
});
const optionsAccess = { rotate_via: "agent-options" };

describe("limit agent rights with agent options", () => {
	afterEach(() => {
		toast.dismiss();
	});

	it("SFTP: asks for the password first, then sends the user and key to agent-options", async () => {
		const mock = mockRepository(sftpDetail, { access: optionsAccess });
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);

		expect(await within(sheet).findByText("COMMANDS")).toBeInTheDocument();
		expect(mock.policy[0]?.get("preset")).toBe("sftp");
		await user.type(within(sheet).getByLabelText("Private key"), "PRIVATE-KEY");
		await user.click(submit(sheet));
		expect(mock.puts).toHaveLength(0);
		await confirmPassword(user);

		expect(
			await screen.findByText(
				"Agents switched to the new key. Disable the old one at SFTP (OpenSSH).",
			),
		).toBeInTheDocument();
		expect(mock.puts).toEqual([
			{
				path: "agent-options",
				body: {
					agent_options: { user: "lodestore-agent", key: "PRIVATE-KEY" },
				},
			},
		]);
	});

	it("cancel step-up -> no PUT for agent-options", async () => {
		const mock = mockRepository(sftpDetail, { access: optionsAccess });
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await user.type(await within(sheet).findByLabelText("Private key"), "K");
		await user.click(submit(sheet));
		const dialog = await screen.findByRole("dialog", {
			name: "Confirm your password",
		});
		await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(mock.puts).toHaveLength(0);
		expect(screen.queryByText(/Agents switched/)).toBeNull();
		expect(within(sheet).queryByRole("alert")).toBeNull();
		expect(sheet).toBeInTheDocument();
	});

	it("a server step_up_required that is cancelled shows neither an alert nor a toast", async () => {
		const mock = mockRepository(sftpDetail, {
			access: optionsAccess,
			reply: () => apiError(403, "step_up_required", "password required"),
		});
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await user.type(await within(sheet).findByLabelText("Private key"), "K");
		await user.click(submit(sheet));
		await confirmPassword(user);
		const again = await screen.findByRole("dialog", {
			name: "Confirm your password",
		});
		await user.click(within(again).getByRole("button", { name: "Cancel" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(mock.puts).toHaveLength(1);
		expect(within(sheet).queryByRole("alert")).toBeNull();
		expect(screen.queryByText(/Agents switched/)).toBeNull();
	});

	it("tells how many online agents are too old to take the new key", async () => {
		mockRepository(sftpDetail, {
			access: optionsAccess,
			replyBody: {
				agent_key_state: "verified",
				guarantee_level: "create_only_enforced",
				needs_update: 2,
				offline: 0,
				online_notified: 1,
			},
		});
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await user.type(await within(sheet).findByLabelText("Private key"), "K");
		await user.click(submit(sheet));
		await confirmPassword(user);

		expect(
			await screen.findByText(
				/2 online agents are too old to take the new key; update them\./,
			),
		).toBeInTheDocument();
	});

	it("GCS sends the service account key", async () => {
		const mock = mockRepository(detailOf("gcs", { bucket: "b", root: "/r" }), {
			access: optionsAccess,
		});
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await user.type(
			await within(sheet).findByLabelText("Service account key"),
			"SA-KEY",
		);
		await user.click(submit(sheet));
		await confirmPassword(user);

		await screen.findByText(/Agents switched to the new key/);
		expect(mock.puts).toEqual([
			{
				path: "agent-options",
				body: { agent_options: { credential: "SA-KEY" } },
			},
		]);
	});

	it("Azure with its own SAS sends the SAS token", async () => {
		const mock = mockRepository(
			detailOf("azblob", { account_name: "acct", container: "c" }),
			{ access: optionsAccess },
		);
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);
		await user.type(
			await within(sheet).findByLabelText("SAS token"),
			"sv=1&sig=x",
		);
		await user.click(submit(sheet));
		await confirmPassword(user);

		await screen.findByText(/Agents switched to the new key/);
		expect(mock.puts).toEqual([
			{
				path: "agent-options",
				body: { agent_options: { sas_token: "sv=1&sig=x" } },
			},
		]);
	});

	it("R2 shows the bucket lock instruction and only a Close button", async () => {
		const mock = mockRepository(
			detailOf("s3", {
				bucket: "b",
				endpoint: "https://abc123.r2.cloudflarestorage.com",
			}),
			{
				access: { after_revoke: "server_key_shared", rotate_via: null },
				rights: { mechanism: "instruction", text: "TURN ON BUCKET LOCK" },
			},
		);
		const user = userEvent.setup();
		const { sheet } = await openSheet(user);

		expect(
			await within(sheet).findByText("R2 tokens cannot be limited"),
		).toBeInTheDocument();
		expect(within(sheet).getByText("TURN ON BUCKET LOCK")).toBeInTheDocument();
		expect(mock.policy[0]?.get("preset")).toBe("cloudflare-r2");
		expect(
			within(sheet).queryByRole("button", { name: "Check and switch agents" }),
		).toBeNull();
		await user.click(
			within(sheet).getAllByRole("button", { name: "Close" })[0],
		);
		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Limit agent rights" }),
			).toBeNull(),
		);
		expect(mock.puts).toHaveLength(0);
	});

	it.each([
		["GCS on shared credentials", "gcs", { bucket: "b" }, "new_repository"],
		[
			"Azure with a SAS issued by the server",
			"azblob",
			{ container: "c" },
			"expires",
		],
	])(
		"%s has no Limit agent rights button",
		async (_name, backend, options, afterRevoke) => {
			mockRepository(detailOf(backend, options), {
				access: { after_revoke: afterRevoke, rotate_via: null },
			});
			renderApp("/settings/repositories/repo-1");

			await screen.findByText("What a host can do to backups");
			await waitFor(() =>
				expect(
					screen.queryByRole("button", { name: "Limit agent rights" }),
				).toBeNull(),
			);
			expect(
				screen.queryByRole("button", { name: "Replace agent key" }),
			).toBeNull();
		},
	);
});

describe("check host key again", () => {
	afterEach(() => {
		toast.dismiss();
	});

	const changed = {
		key: "ssh-ed25519 NEWKEY",
		fingerprint: "SHA256:new-fingerprint",
		pinned_fingerprint: "SHA256:old-fingerprint",
		matches: false,
	};

	it("is offered for SFTP only", async () => {
		mockRepository(s3Detail);
		renderApp("/settings/repositories/repo-1");
		await screen.findByText("What a host can do to backups");
		expect(
			screen.queryByRole("button", { name: "Check host key again" }),
		).toBeNull();
	});

	it("says so when the server presents the pinned key", async () => {
		const mock = mockRepository(sftpDetail, {
			access: optionsAccess,
			offer: {
				...changed,
				matches: true,
				fingerprint: "SHA256:old-fingerprint",
			},
		});
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Check host key again" }),
		);

		expect(
			await screen.findByText("The server presents the pinned key."),
		).toBeInTheDocument();
		expect(mock.hostKey.checks).toBe(1);
		expect(screen.queryByRole("dialog")).toBeNull();
	});

	it("host key changed dialog keeps the pinned key without a request", async () => {
		const mock = mockRepository(sftpDetail, {
			access: optionsAccess,
			offer: changed,
		});
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Check host key again" }),
		);

		const dialog = await screen.findByRole("dialog", {
			name: "The server's host key changed",
		});
		expect(
			within(dialog).getByText("SHA256:old-fingerprint"),
		).toBeInTheDocument();
		expect(
			within(dialog).getByText("SHA256:new-fingerprint"),
		).toBeInTheDocument();
		await user.click(
			within(dialog).getByRole("button", { name: "Keep the pinned key" }),
		);

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(mock.hostKey.puts).toHaveLength(0);
	});

	it("host key changed dialog trusts the new key", async () => {
		const mock = mockRepository(sftpDetail, {
			access: optionsAccess,
			offer: changed,
		});
		renderApp("/settings/repositories/repo-1");
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Check host key again" }),
		);
		const dialog = await screen.findByRole("dialog", {
			name: "The server's host key changed",
		});
		await user.click(
			within(dialog).getByRole("button", { name: "Trust the new key" }),
		);

		expect(
			await screen.findByText("Agents now trust the new host key."),
		).toBeInTheDocument();
		expect(mock.hostKey.puts).toEqual([{ host_key: "ssh-ed25519 NEWKEY" }]);
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
	});
});
