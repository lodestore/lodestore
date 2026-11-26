import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { Host } from "@/api/gen/model";
import { formatDateTime } from "@/lib/format";
import { agentUpdateStatus, mockAgentUpdates } from "@/test/agent-update";
import { renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const host: Host = {
	id: "web10000-0000-4000-8000-000000000000",
	hostname: "web-1",
	os: "linux",
	arch: "arm64",
	distro: null,
	version: "0.1.0",
	status: "online",
	labels: [],
	repository_id: "repo-1",
	enrolled_at: Date.now() - 86_400_000,
	last_seen_at: Date.now() - 20_000,
	cert_not_after: Date.now() + 86_400_000,
};

const accessOf = (patch: Record<string, unknown>) => ({
	after_revoke: "until_rotation",
	status: "revoked_hosts_hold_access",
	rotate_via: "agent-key",
	access_ends_by: null,
	hosts_need_agent_update: 0,
	revoked_hosts_before_rotation: 0,
	revoked_hosts_since_rotation: 1,
	...patch,
});

function mockRevoke(access: Record<string, unknown> | "error") {
	let accessCalls = 0;
	server.use(
		http.get("/api/v1/hosts", () => HttpResponse.json([host])),
		http.post(
			`/api/v1/hosts/${host.id}/revoke`,
			() => new HttpResponse(null, { status: 204 }),
		),
		http.get("/api/v1/repositories/repo-1/storage-access", () => {
			accessCalls += 1;
			return access === "error"
				? HttpResponse.json({ code: "x", message: "boom" }, { status: 500 })
				: HttpResponse.json(accessOf(access));
		}),
		http.get("/api/v1/repositories/repo-1", () =>
			HttpResponse.json({
				...repository,
				id: "repo-1",
				name: "main",
				location: { backend: "s3", options: { bucket: "b" } },
				checks: [],
			}),
		),
	);
	return { accessCalls: () => accessCalls };
}

async function revokeHost() {
	useSignedIn();
	renderApp("/hosts");
	await screen.findByRole("heading", { name: "Hosts" });
	const user = userEvent.setup();
	await user.click(
		await screen.findByRole("button", { name: "Actions for web-1" }),
	);
	await user.click(
		await screen.findByRole("menuitem", { name: "Revoke host" }),
	);
	const dialog = await screen.findByRole("alertdialog");
	await user.click(within(dialog).getByRole("button", { name: "Revoke host" }));
	return { user, dialog };
}

beforeEach(() => mockAgentUpdates(agentUpdateStatus()));
afterEach(() => toast.dismiss());

describe("revoke host follow-up", () => {
	it("offers to replace the agent key when the revoked host can still write", async () => {
		mockRevoke({});
		const { user, dialog } = await revokeHost();

		expect(
			await within(dialog).findByText(
				"web-1 can still write to main until the agent key is replaced.",
			),
		).toBeInTheDocument();
		expect(
			within(dialog).getByRole("button", { name: "Later" }),
		).toBeInTheDocument();
		await user.click(
			within(dialog).getByRole("button", { name: "Replace agent key" }),
		);

		expect(
			await screen.findByRole("dialog", { name: "Limit agent rights" }),
		).toBeInTheDocument();
		expect(screen.queryByRole("alertdialog")).toBeNull();
	});

	it("Later closes the dialog and opens nothing", async () => {
		mockRevoke({});
		const { user, dialog } = await revokeHost();
		await user.click(
			await within(dialog).findByRole("button", { name: "Later" }),
		);

		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(
			screen.queryByRole("dialog", { name: "Limit agent rights" }),
		).toBeNull();
		expect(screen.getAllByText('Host "web-1" revoked.').length).toBeGreaterThan(
			0,
		);
	});

	it("says when the access ends and offers no replacement", async () => {
		const endsBy = Date.now() + 3_600_000;
		mockRevoke({
			after_revoke: "expires",
			status: "access_expires",
			rotate_via: null,
			access_ends_by: endsBy,
		});
		const { dialog } = await revokeHost();

		expect(
			await within(dialog).findByText(
				`Its access ends by ${formatDateTime(endsBy)}.`,
			),
		).toBeInTheDocument();
		expect(
			within(dialog).queryByRole("button", { name: "Replace agent key" }),
		).toBeNull();
		expect(
			within(dialog).getByRole("button", { name: "Close" }),
		).toBeInTheDocument();
	});

	it("server_key_shared offers no key replacement", async () => {
		mockRevoke({ after_revoke: "server_key_shared", rotate_via: null });
		const { dialog } = await revokeHost();

		expect(
			await within(dialog).findByText(
				"web-1 holds the server's own credentials for main. Only new credentials at the provider close that.",
			),
		).toBeInTheDocument();
		expect(
			within(dialog).queryByRole("button", { name: "Replace agent key" }),
		).toBeNull();
		expect(within(dialog).queryByText(/can still write/)).toBeNull();
	});

	it.each([
		["nothing is held", { status: "no_revoked_hosts" }],
		["the access check fails", "error" as const],
	])("closes as before when %s", async (_name, access) => {
		const mock = mockRevoke(access);
		await revokeHost();

		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(mock.accessCalls()).toBe(1);
		expect(screen.getAllByText('Host "web-1" revoked.').length).toBeGreaterThan(
			0,
		);
	});
});
