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
};

function mockRepository(
	detail: ReturnType<typeof detailOf>,
	options: {
		access?: Record<string, unknown>;
		reply?: () => Response;
		replyBody?: Record<string, unknown>;
	} = {},
): Mock {
	const mock: Mock = { puts: [], policy: [], stepUps: 0 };
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
					mechanism: "policy",
					note: "",
					preset: new URL(request.url).searchParams.get("preset"),
					server_text: null,
					text: "COMMANDS",
					verified: true,
				},
			});
		}),
		http.put("/api/v1/repositories/repo-1/agent-key", put("agent-key")),
		http.put("/api/v1/repositories/repo-1/agent-options", put("agent-options")),
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
