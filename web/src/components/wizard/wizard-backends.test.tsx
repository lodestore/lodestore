import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import type { CheckResult } from "@/api/gen/model";
import { copy } from "@/copy";
import { renderApp, useSignedIn } from "@/test/app";
import { rightsFor, server } from "@/test/server";
import { bucketFacts } from "./probe-result-panel";

afterEach(() => {
	cleanup();
	localStorage.clear();
	sessionStorage.clear();
});

type User = ReturnType<typeof userEvent.setup>;
type Body = {
	location: { backend: string; options: Record<string, string> };
	agent_options?: Record<string, string>;
	agent_credentials?: unknown;
	server_credentials?: unknown;
};

const CHECKS = [
	{ id: "write", status: "pass" },
	{ id: "list", status: "pass" },
	{ id: "conditional_write", status: "pass" },
	{ id: "agent_delete", status: "pass" },
	{ id: "object_lock", status: "neutral" },
];

const GOOD = {
	existing_repository: false,
	blocking: false,
	agent_key_state: "verified",
	guarantee_level: "create_only_enforced",
	guarantee_notes: [] as string[],
	checks: CHECKS,
};

const WEAK = {
	...GOOD,
	agent_key_state: "can_delete",
	guarantee_level: "host_can_overwrite_or_delete",
	guarantee_notes: ["shared_credentials"],
	checks: CHECKS.map((c) =>
		c.id === "agent_delete" ? { ...c, status: "warn" } : c,
	),
};

const HOST_KEY_ONE = {
	key: "ssh-ed25519 AAAAC3NzaHOSTKEYONE",
	fingerprint: "SHA256:fingerprintone",
};
const HOST_KEY_TWO = {
	key: "ssh-ed25519 AAAAC3NzaHOSTKEYTWO",
	fingerprint: "SHA256:fingerprinttwo",
};

const offer = (hostKey: typeof HOST_KEY_ONE) => ({
	existing_repository: false,
	blocking: false,
	agent_key_state: "unchecked",
	guarantee_level: "unchecked",
	guarantee_notes: [],
	checks: [],
	host_key: hostKey,
});

function mockServer(respond: (body: Body) => object = () => GOOD) {
	const bodies: Body[] = [];
	const policies: URL[] = [];
	useSignedIn({ repositories: [] });
	server.use(
		http.get("/api/v1/repositories", () => HttpResponse.json([])),
		http.post("/api/v1/repositories/check", async ({ request }) => {
			const body = (await request.json()) as Body;
			bodies.push(body);
			return HttpResponse.json(respond(body) as Record<string, unknown>);
		}),
		http.get("/api/v1/agent-policy", ({ request }) => {
			const url = new URL(request.url);
			policies.push(url);
			return HttpResponse.json({
				iam_json: "",
				minio_commands: "",
				rights: rightsFor(
					url.searchParams.get("preset") ?? "aws-s3",
					url.searchParams.get("bucket") ?? "",
				),
			});
		}),
		http.post("/api/v1/repositories/wizard", () =>
			HttpResponse.json(
				{
					wizard_id: "w1",
					recovery_key: "lode-rk1-aaaa-bbbb-wxyz",
					expires_at: Date.now() + 1_800_000,
				},
				{ status: 201 },
			),
		),
	);
	return { bodies, policies };
}

async function chooseTile(user: User, tile: RegExp) {
	await user.click(await screen.findByRole("radio", { name: tile }));
	await user.click(
		screen.getByRole("button", { name: "Continue to connection" }),
	);
}

const stored = () => JSON.stringify(sessionStorage);
const toRights = () =>
	screen.getByRole("button", { name: "Continue to agent rights" });

describe("Google Cloud Storage", () => {
	async function connect(user: User) {
		await chooseTile(user, /Google Cloud Storage/);
		await user.type(await screen.findByLabelText("Bucket"), "my-bucket");
		await user.type(screen.getByLabelText("Folder"), "lode");
		await user.type(
			screen.getByLabelText("Service account key"),
			"SERVER-GCS-KEY",
		);
		await user.click(toRights());
		await screen.findByRole("heading", {
			name: "Give agents a key that cannot delete",
		});
	}

	it("goes from the tile to the recovery key with two different keys", async () => {
		const { bodies, policies } = mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");
		await connect(user);

		expect(policies.at(-1)?.searchParams.get("preset")).toBe(
			"google-cloud-storage",
		);
		expect(policies.at(-1)?.searchParams.get("bucket")).toBe("my-bucket");
		expect(
			screen.getByText("agent-rights google-cloud-storage my-bucket"),
		).toBeInTheDocument();

		await user.type(
			screen.getByLabelText("Service account key"),
			"AGENT-GCS-KEY",
		);
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(bodies[0].location).toEqual({
			backend: "gcs",
			options: {
				bucket: "my-bucket",
				root: "lode",
				credential: "SERVER-GCS-KEY",
			},
		});
		expect(bodies[0].agent_options).toEqual({ credential: "AGENT-GCS-KEY" });
		expect(bodies[0].agent_credentials).toBeUndefined();
		expect(bodies[0].server_credentials).toBeUndefined();

		await user.click(
			await screen.findByRole("button", { name: "Continue to recovery key" }),
		);
		expect(
			await screen.findByRole("heading", { name: copy.recoveryKey.heading }),
		).toBeInTheDocument();
		expect(stored()).not.toContain("SERVER-GCS-KEY");
		expect(stored()).not.toContain("AGENT-GCS-KEY");
	});

	it("sends no agent options after Skip and says the credentials are shared", async () => {
		const { bodies } = mockServer(() => WEAK);
		const user = userEvent.setup();
		renderApp("/welcome");
		await connect(user);

		await user.click(
			screen.getByRole("button", { name: "Skip, use the server key" }),
		);
		expect(
			await screen.findByText(copy.rights.skippedBodyShared),
		).toBeInTheDocument();
		expect(bodies).toHaveLength(1);
		expect(bodies[0].agent_options).toBeUndefined();
		expect(bodies[0].location.options.credential).toBe("SERVER-GCS-KEY");
		expect(stored()).not.toContain("SERVER-GCS-KEY");
	});
});

describe("SFTP host key", () => {
	async function address(user: User, tile = /SFTP \(OpenSSH\)/) {
		await chooseTile(user, tile);
		await user.type(await screen.findByLabelText("Address"), "sftp.test:22");
		await user.type(screen.getByLabelText("Directory"), "/srv/lodestore");
	}

	async function serverKey(user: User) {
		await user.type(screen.getByLabelText("User"), "lodestore");
		await user.type(screen.getByLabelText("Private key"), "SERVER-PRIVATE-KEY");
	}

	const getKey = () => screen.getByRole("button", { name: "Get host key" });
	const matches = () => screen.getByRole("radio", { name: "It matches" });

	function respondOffer(...keys: (typeof HOST_KEY_ONE)[]) {
		let n = 0;
		return (body: Body) =>
			body.location.options.key
				? GOOD
				: offer(keys[Math.min(n++, keys.length - 1)]);
	}

	it("keeps Get host key off until there is an address", async () => {
		mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");
		await chooseTile(user, /SFTP \(OpenSSH\)/);
		expect(
			await screen.findByRole("button", { name: "Get host key" }),
		).toBeDisabled();
		await user.type(screen.getByLabelText("Address"), "sftp.test:22");
		expect(getKey()).toBeEnabled();
	});

	it("offer request body has no user/private key", async () => {
		const { bodies } = mockServer(respondOffer(HOST_KEY_ONE));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);

		await user.click(getKey());
		await screen.findByText("Is this your server?");
		expect(bodies).toEqual([
			{
				location: {
					backend: "sftp",
					options: { endpoint: "sftp.test:22", root: "/srv/lodestore" },
				},
			},
		]);
		expect(
			screen.getByText("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub"),
		).toBeInTheDocument();
		expect(
			screen.getByText("SHA256:fingerprintone (ED25519)"),
		).toBeInTheDocument();
		expect(toRights()).toBeDisabled();
	});

	it("mismatch keeps Continue disabled", async () => {
		mockServer(respondOffer(HOST_KEY_ONE));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);
		await user.click(getKey());
		await screen.findByText("Is this your server?");

		await user.click(matches());
		expect(toRights()).toBeEnabled();
		await user.click(screen.getByRole("radio", { name: "It does not match" }));
		expect(toRights()).toBeDisabled();
		expect(
			screen.getByText(/^Do not continue: this may not be your server/),
		).toBeInTheDocument();
	});

	it("endpoint or root change after the offer drops the confirmed key", async () => {
		const { bodies } = mockServer(respondOffer(HOST_KEY_ONE, HOST_KEY_TWO));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);
		await user.click(getKey());
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		expect(toRights()).toBeEnabled();

		await user.type(screen.getByLabelText("User"), "x");
		await user.type(screen.getByLabelText("Private key"), "x");
		expect(toRights()).toBeEnabled();
		expect(screen.getByText("Is this your server?")).toBeInTheDocument();

		await user.type(screen.getByLabelText("Directory"), "/more");
		expect(screen.queryByText("Is this your server?")).toBeNull();
		expect(toRights()).toBeDisabled();

		await user.type(screen.getByLabelText("Address"), "0");
		await user.click(getKey());
		expect(await screen.findByText(/fingerprinttwo/)).toBeInTheDocument();
		expect(bodies[1].location.options).toEqual({
			endpoint: "sftp.test:220",
			root: "/srv/lodestore/more",
		});
		expect(toRights()).toBeDisabled();

		await user.click(matches());
		await user.click(toRights());
		await user.type(
			await screen.findByLabelText("Private key"),
			"AGENT-PRIVATE-KEY",
		);
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(3));
		expect(bodies[2].location.options.host_key).toBe(HOST_KEY_TWO.key);
		expect(bodies[2].location.options.user).toBe("lodestorex");
		expect(bodies[2].agent_options).toEqual({
			user: "lodestore-agent",
			key: "AGENT-PRIVATE-KEY",
		});
	});

	it("the first contact does not open the key step", async () => {
		const { bodies } = mockServer(respondOffer(HOST_KEY_ONE));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);
		await user.click(getKey());
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		await user.click(toRights());
		await screen.findByRole("heading", {
			name: "Give agents a key that cannot delete",
		});

		expect(bodies).toHaveLength(1);
		expect(
			screen.getByRole("button", { name: "Continue to recovery key" }),
		).toBeDisabled();
		expect(screen.getByText(copy.rights.resultIdle)).toBeInTheDocument();
	});

	it("shows the error when the server cannot be reached", async () => {
		mockServer(() => ({
			...offer(HOST_KEY_ONE),
			host_key: null,
			checks: [{ id: "write", status: "fail", error: "connection refused" }],
		}));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await user.click(getKey());

		const alert = await screen.findByRole("alert");
		expect(alert).toHaveTextContent("Connection refused");
		expect(screen.queryByText("Is this your server?")).toBeNull();
	});

	it("never stores the host key or private keys", async () => {
		const { bodies } = mockServer(respondOffer(HOST_KEY_ONE));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);
		await user.click(getKey());
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		await user.click(toRights());
		await user.type(
			await screen.findByLabelText("Private key"),
			"AGENT-PRIVATE-KEY",
		);
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(2));

		for (const secret of [
			"AAAAC3NzaHOSTKEYONE",
			"fingerprintone",
			"SERVER-PRIVATE-KEY",
			"AGENT-PRIVATE-KEY",
		]) {
			expect(stored()).not.toContain(secret);
		}
		expect(localStorage.length).toBe(0);
	});

	it("builds the agent commands for the agent user with a delay", async () => {
		const { policies } = mockServer(respondOffer(HOST_KEY_ONE));
		const user = userEvent.setup();
		renderApp("/welcome");
		await address(user);
		await serverKey(user);
		await user.click(getKey());
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		await user.click(toRights());
		await screen.findByText("agent-rights sftp");

		const first = policies.at(-1);
		expect(first?.searchParams.get("preset")).toBe("sftp");
		expect(first?.searchParams.get("prefix")).toBe("/srv/lodestore");
		expect(first?.searchParams.get("user")).toBe("lodestore-agent");

		await user.type(screen.getByLabelText("User"), "bob");
		await waitFor(() =>
			expect(policies.at(-1)?.searchParams.get("user")).toBe("bob"),
		);
		expect(
			policies.filter((p) => p.searchParams.get("user")?.startsWith("b")),
		).toHaveLength(1);
	});
});

describe("Hetzner Storage Box", () => {
	it("derives the address and user from the template and shows instructions", async () => {
		const { bodies, policies } = mockServer((body) =>
			body.location.options.key ? GOOD : offer(HOST_KEY_ONE),
		);
		const user = userEvent.setup();
		renderApp("/welcome");
		await chooseTile(user, /Hetzner Storage Box/);

		expect(
			await screen.findByLabelText("Storage Box user"),
		).toBeInTheDocument();
		expect(screen.queryByLabelText("Address")).toBeNull();
		expect(screen.queryByLabelText("User")).toBeNull();
		await user.type(screen.getByLabelText("Storage Box user"), "u123");
		expect(screen.getByText("u123.your-storagebox.de:23")).toBeInTheDocument();
		await user.type(screen.getByLabelText("Directory"), "/backups");
		await user.type(screen.getByLabelText("Private key"), "BOX-KEY");

		await user.click(screen.getByRole("button", { name: "Get host key" }));
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		expect(bodies[0].location.options).toEqual({
			endpoint: "u123.your-storagebox.de:23",
			root: "/backups",
		});
		await user.click(toRights());

		expect(
			await screen.findByText("agent-rights hetzner-storage-box"),
		).toBeInTheDocument();
		expect(
			screen.queryByRole("region", { name: "Commands for the new key" }),
		).toBeNull();
		expect(policies.at(-1)?.searchParams.get("user")).toBeNull();

		await user.type(screen.getByLabelText("User"), "u123-sub1");
		await user.type(screen.getByLabelText("Private key"), "SUB-KEY");
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(2));
		expect(bodies[1].location.options).toEqual({
			endpoint: "u123.your-storagebox.de:23",
			user: "u123",
			key: "BOX-KEY",
			root: "/backups",
			host_key: HOST_KEY_ONE.key,
		});
		expect(bodies[1].agent_options).toEqual({
			user: "u123-sub1",
			key: "SUB-KEY",
		});
	});
});

describe("Azure Blob Storage", () => {
	async function connect(user: User, kind: "Account key" | "SAS token") {
		await chooseTile(user, /Azure Blob Storage/);
		await user.type(
			await screen.findByLabelText("Storage account name"),
			"acct",
		);
		await user.type(screen.getByLabelText("Container"), "lode");
		await user.type(screen.getByLabelText("Folder"), "backups");
		if (kind === "SAS token") {
			await user.click(screen.getByRole("radio", { name: "SAS token" }));
		}
		await user.type(screen.getByLabelText(kind), "SERVER-SECRET");
		await user.click(toRights());
		await screen.findByRole("heading", {
			name: "Give agents a key that cannot delete",
		});
	}

	it("lets the server sign the SAS when it holds the account key", async () => {
		const { bodies } = mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");
		await chooseTile(user, /Azure Blob Storage/);
		expect(
			await screen.findByText(
				"Optional. Keeps the repository in a folder of the container.",
			),
		).toBeInTheDocument();
		expect(screen.getByText("Container and server key")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Previous step" }));
		await connect(user, "Account key");

		expect(screen.getByText(copy.rights.azure)).toBeInTheDocument();
		expect(screen.queryByLabelText("SAS token")).toBeNull();
		expect(
			screen.queryByRole("button", { name: "Skip, use the server key" }),
		).toBeNull();
		const check = screen.getByRole("button", { name: "Check" });
		expect(check).toBeEnabled();
		await user.click(check);
		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(bodies[0].location).toEqual({
			backend: "azblob",
			options: {
				account_name: "acct",
				container: "lode",
				root: "backups",
				account_key: "SERVER-SECRET",
			},
		});
		expect(bodies[0].agent_options).toBeUndefined();
		expect(stored()).not.toContain("SERVER-SECRET");
	});

	it("sends the agent SAS when the server holds a SAS", async () => {
		const { bodies } = mockServer();
		const user = userEvent.setup();
		renderApp("/welcome");
		await connect(user, "SAS token");

		expect(
			await screen.findByText("agent-rights azure-blob"),
		).toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Check" })).toBeDisabled();
		expect(
			screen.getByRole("button", { name: "Skip, use the server key" }),
		).toBeVisible();
		await user.type(screen.getByLabelText("SAS token"), "AGENT-SAS");
		await user.click(screen.getByRole("button", { name: "Check" }));
		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(bodies[0].location.options.sas_token).toBe("SERVER-SECRET");
		expect(bodies[0].location.options.account_key).toBeUndefined();
		expect(bodies[0].agent_options).toEqual({ sas_token: "AGENT-SAS" });
		expect(stored()).not.toContain("AGENT-SAS");
	});
});

const result = (
	level: string,
	notes: string[] = [],
	extra: Partial<CheckResult> = {},
): CheckResult => ({
	...GOOD,
	guarantee_level: level,
	guarantee_notes: notes,
	...extra,
});

describe("bucketFacts", () => {
	const statuses = (r: CheckResult, backend = "s3") =>
		bucketFacts(r, backend).map((f) => `${f.id}:${f.status}`);

	it("reads versioning from the level and the overwrite note", () => {
		expect(statuses(result("overwrite_keeps_old_version"))).toEqual([
			"versioning:pass",
			"lifecycle:pass",
		]);
		expect(
			statuses(result("host_can_overwrite_or_delete", ["agent_can_overwrite"])),
		).toEqual(["versioning:warn", "lifecycle:pass"]);
		expect(statuses(result("create_only_enforced"))).toEqual([
			"lifecycle:pass",
		]);
	});

	it("warns about lifecycle rules with the note text and never says OK unmeasured", () => {
		const deletes = bucketFacts(
			result("create_only_enforced", ["lifecycle_deletes"]),
			"gcs",
		);
		expect(deletes).toHaveLength(1);
		expect(deletes[0]).toMatchObject({ id: "lifecycle", status: "warn" });
		expect(deletes[0].detail).toBe(copy.guarantee.notes.lifecycle_deletes);
		expect(
			statuses(result("create_only_enforced", ["lifecycle_unverified"])),
		).toEqual(["lifecycle:warn"]);
		expect(statuses(result("unchecked"), "azblob")).toEqual([]);
	});

	it("is empty for sftp and local", () => {
		expect(statuses(result("overwrite_keeps_old_version"), "sftp")).toEqual([]);
		expect(statuses(result("overwrite_keeps_old_version"), "local")).toEqual(
			[],
		);
	});
});

describe("Bucket group in the Result panel", () => {
	it("shows Object Lock, Versioning and Lifecycle for B2 without repeating the note", async () => {
		mockServer(() =>
			result("overwrite_keeps_old_version", ["lifecycle_deletes"], {
				agent_key_state: "can_overwrite",
			}),
		);
		const user = userEvent.setup();
		renderApp("/welcome");
		await chooseTile(user, /Backblaze B2/);
		await user.type(await screen.findByLabelText("Region"), "eu-central-003");
		await user.type(screen.getByLabelText("Bucket"), "my-bucket");
		await user.type(screen.getByLabelText("Server key: Access key ID"), "root");
		await user.type(
			screen.getByLabelText("Server key: Secret access key"),
			"SECRET",
		);
		await user.click(toRights());
		await user.type(
			await screen.findByLabelText("Agent key (write-only): Access key ID"),
			"agent",
		);
		await user.type(
			screen.getByLabelText("Agent key (write-only): Secret access key"),
			"AGENT",
		);
		await user.click(screen.getByRole("button", { name: "Check" }));

		await screen.findByRole("heading", { name: "Bucket" });
		const rows = document.querySelectorAll("[data-fact]");
		expect([...rows].map((r) => r.getAttribute("data-fact"))).toEqual([
			"versioning",
			"lifecycle",
		]);
		expect(screen.getByText("Object Lock")).toBeInTheDocument();
		expect(
			screen.getByText(copy.rights.facts.versioningOn),
		).toBeInTheDocument();
		expect(
			screen.getAllByText(copy.guarantee.notes.lifecycle_deletes),
		).toHaveLength(1);
	});

	it("keeps Object Lock in the main list for sftp", async () => {
		mockServer((body) =>
			body.location.options.key ? GOOD : offer(HOST_KEY_ONE),
		);
		const user = userEvent.setup();
		renderApp("/welcome");
		await chooseTile(user, /SFTP \(OpenSSH\)/);
		await user.type(await screen.findByLabelText("Address"), "sftp.test:22");
		await user.type(screen.getByLabelText("Directory"), "/srv");
		await user.type(screen.getByLabelText("User"), "lodestore");
		await user.type(screen.getByLabelText("Private key"), "KEY");
		await user.click(screen.getByRole("button", { name: "Get host key" }));
		await user.click(await screen.findByRole("radio", { name: "It matches" }));
		await user.click(toRights());
		await user.type(await screen.findByLabelText("Private key"), "AGENT");
		await user.click(screen.getByRole("button", { name: "Check" }));

		await screen.findByText("Object Lock");
		expect(screen.queryByRole("heading", { name: "Bucket" })).toBeNull();
		expect(document.querySelectorAll("[data-fact]")).toHaveLength(0);
	});
});
