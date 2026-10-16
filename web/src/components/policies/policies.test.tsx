import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import {
	configure,
	render,
	screen,
	waitFor,
	within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	BackupPolicy,
	JobSummary,
	MatchHost,
	PolicyDetail,
	PolicySummary,
	ValidateRequest,
	ValidateResponse,
} from "@/api/gen/model";
import { PoliciesTable } from "@/components/policies/policies-table";
import {
	draftFromPolicy,
	policyFromDraft,
} from "@/components/policies/policy-form";
import { ThemeProvider } from "@/components/theme/theme-provider";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

configure({ asyncUtilTimeout: 5000 });

function matchHost(name: string, over: Partial<MatchHost> = {}): MatchHost {
	return {
		id: `${name}-id`,
		hostname: name,
		online: true,
		repository_id: "repo-1",
		repository_name: "main",
		...over,
	};
}

function makePolicy(
	name: string,
	spec: Partial<BackupPolicy["spec"]> = {},
): BackupPolicy {
	return {
		apiVersion: "lodestore.io/v1alpha1",
		kind: "BackupPolicy",
		metadata: { name },
		spec: {
			repository: "main",
			selector: { matchLabels: {} },
			paths: ["/etc", "/home"],
			exclude: ["**/.cache", "**/node_modules"],
			...spec,
		},
	};
}

function makeDetail(
	name: string,
	over: Partial<PolicyDetail> = {},
	spec: Partial<BackupPolicy["spec"]> = {},
): PolicyDetail {
	return {
		name,
		policy: makePolicy(name, spec),
		yaml: `kind: BackupPolicy\nmetadata:\n  name: ${name}\n`,
		revision: 1,
		created_at: 0,
		updated_at: 0,
		repository_id: "repo-1",
		repository_name: "main",
		matching: {
			hosts: [matchHost("web-1"), matchHost("db-1")],
			other_repository: [],
		},
		warnings: [],
		...over,
	};
}

type Handler = (body: ValidateRequest) => ValidateResponse;

function validResponse(
	over: Partial<ValidateResponse> = {},
	hosts = [matchHost("web-1"), matchHost("db-1")],
): ValidateResponse {
	return {
		valid: true,
		errors: [],
		name_taken: false,
		warnings: [],
		yaml: "kind: BackupPolicy\n",
		policy: makePolicy("etc-and-home"),
		matching: { hosts, other_repository: [] },
		...over,
	};
}

function mockValidate(handler: Handler = () => validResponse()) {
	const requests: ValidateRequest[] = [];
	server.use(
		http.post("/api/v1/policies/validate", async ({ request }) => {
			const body = (await request.json()) as ValidateRequest;
			requests.push(body);
			return HttpResponse.json(handler(body));
		}),
	);
	return requests;
}

function mockPolicy(detail: PolicyDetail) {
	server.use(
		http.get("/api/v1/jobs", () =>
			HttpResponse.json({ items: [], page: 1, page_size: 50, total: 0 }),
		),
		http.get(`/api/v1/policies/${detail.name}`, () =>
			HttpResponse.json(detail),
		),
	);
}

describe("New policy", () => {
	it("saves the quickstart policy and runs it in two clicks", async () => {
		useSignedIn();
		mockValidate();
		const created: unknown[] = [];
		const runs: string[] = [];
		const saved = makeDetail("etc-and-home");
		server.use(
			http.post("/api/v1/policies", async ({ request }) => {
				created.push(await request.json());
				return HttpResponse.json(saved, { status: 201 });
			}),
			http.get("/api/v1/policies/etc-and-home", () => HttpResponse.json(saved)),
			http.post("/api/v1/policies/etc-and-home/run", () => {
				runs.push("etc-and-home");
				return HttpResponse.json({
					jobs: [
						{ id: "job-1", host_id: "a", hostname: "web-1", created: true },
						{ id: "job-2", host_id: "b", hostname: "db-1", created: true },
					],
				});
			}),
		);
		const user = userEvent.setup();
		renderApp("/policies/new");

		await screen.findByRole("heading", { name: "New policy" });
		expect(screen.getByLabelText("Name")).toHaveValue("etc-and-home");
		expect(
			screen.getByRole("combobox", { name: "Repository" }),
		).toHaveTextContent("main");
		expect(
			screen
				.getAllByRole("textbox", { name: "Paths" })
				.map((el) => (el as HTMLInputElement).value),
		).toEqual(["/etc", "/home"]);
		expect(screen.getByLabelText("Exclude")).toHaveValue(
			"**/.cache\n**/node_modules",
		);

		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);

		await waitFor(() => expect(created).toHaveLength(1));
		expect(created[0]).toMatchObject({
			policy: {
				metadata: { name: "etc-and-home" },
				spec: {
					repository: "main",
					paths: ["/etc", "/home"],
					exclude: ["**/.cache", "**/node_modules"],
				},
			},
		});
		const heading = await screen.findByRole("heading", {
			name: "etc-and-home",
		});
		await waitFor(() => expect(heading).toHaveFocus());

		await user.click(screen.getByRole("button", { name: "Run now" }));
		const dialog = await screen.findByRole("dialog", {
			name: 'Run "etc-and-home" now?',
		});
		const confirm = await within(dialog).findByRole("button", {
			name: "Run on 2 hosts",
		});
		await user.click(confirm);

		expect(await screen.findByText("2 jobs queued.")).toBeInTheDocument();
		expect(runs).toEqual(["etc-and-home"]);
	});
});

const breadcrumbLink = () =>
	within(screen.getByRole("navigation", { name: "breadcrumb" })).getByRole(
		"link",
		{ name: "Policies" },
	);

async function openNew() {
	useSignedIn();
	const user = userEvent.setup();
	renderApp("/policies/new");
	await screen.findByRole("heading", { name: "New policy" });
	return user;
}

async function typeYaml(
	user: ReturnType<typeof userEvent.setup>,
	text: string,
) {
	await user.click(screen.getByRole("tab", { name: "YAML" }));
	const field = await screen.findByRole("textbox", { name: "Policy YAML" });
	await user.clear(field);
	await user.click(field);
	await user.paste(text);
	return field;
}

describe("Policy editor tabs", () => {
	it("validates form edits after a pause and shows the server YAML", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `paths: ${body.policy?.spec.paths.join(",")}\n`,
			}),
		);
		const user = await openNew();
		await waitFor(() => expect(requests).toHaveLength(1));

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "/srv");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.paths).toEqual(["/srv", "/home"]),
		);

		await user.click(screen.getByRole("tab", { name: "YAML" }));
		const yaml = await screen.findByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(yaml).toHaveValue("paths: /srv,/home\n"));
		expect(yaml).toHaveAttribute("spellcheck", "false");
		expect(
			screen.getByText(
				"Editing in the form rewrites the YAML and removes comments.",
			),
		).toBeInTheDocument();
	});

	it("never opens the YAML tab with a document older than the form", async () => {
		mockValidate((body) =>
			validResponse({
				yaml: `paths: ${body.policy?.spec.paths.join(",")}\n`,
			}),
		);
		const user = await openNew();
		await screen.findByText("2 hosts will be backed up");

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.type(first, "/x");
		await user.click(screen.getByRole("tab", { name: "YAML" }));

		const yaml = await screen.findByRole("textbox", { name: "Policy YAML" });
		expect(yaml).toHaveValue("paths: /etc/x,/home\n");
	});

	it("stays on the form when it is invalid and says why", async () => {
		mockValidate((body) =>
			body.policy?.spec.paths.some((p) => !p.startsWith("/"))
				? {
						valid: false,
						errors: [{ path: "spec.paths[0]", message: "Bad path." }],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "var");
		await screen.findByText("Bad path.");

		await user.click(screen.getByRole("tab", { name: "YAML" }));

		expect(
			await screen.findByText(
				"Fix the errors in the form before switching to YAML.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("tab", { name: "Form" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	}, 15_000);

	it("lists YAML errors by line and refuses to switch back to the form", async () => {
		mockValidate((body) =>
			body.yaml?.includes("schedule")
				? {
						valid: false,
						errors: [{ line: 4, message: "unknown field `schedule`" }],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		const field = await typeYaml(user, "spec:\n  schedule: daily\n");

		expect(
			await screen.findByText("Line 4: unknown field `schedule`"),
		).toBeInTheDocument();
		await user.click(screen.getByRole("tab", { name: "Form" }));

		expect(
			await screen.findByText(
				"Fix the YAML errors before switching to the form.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("tab", { name: "YAML" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
		await waitFor(() => expect(field).toHaveFocus());
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
		expect(
			screen.getByText("Fix the errors in the policy to see matching hosts."),
		).toBeInTheDocument();
	});

	it("carries a valid YAML edit over to the form", async () => {
		mockValidate((body) =>
			body.yaml
				? validResponse({
						policy: makePolicy("etc-and-home", { paths: ["/srv"] }),
					})
				: validResponse(),
		);
		const user = await openNew();
		await typeYaml(user, "kind: BackupPolicy\n");
		await user.click(screen.getByRole("tab", { name: "Form" }));

		await waitFor(() =>
			expect(
				screen
					.getAllByRole("textbox", { name: "Paths" })
					.map((el) => (el as HTMLInputElement).value),
			).toEqual(["/srv"]),
		);
	});

	it("shows field errors under the field", async () => {
		mockValidate((body) =>
			body.policy?.spec.paths.some((p) => !p.startsWith("/"))
				? {
						valid: false,
						errors: [
							{
								path: "spec.paths[0]",
								message: 'Enter an absolute path that starts with "/".',
							},
						],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "var");

		const message = await screen.findByText(
			'Enter an absolute path that starts with "/".',
		);
		await waitFor(() => expect(first).toHaveAttribute("aria-invalid", "true"));
		expect(first.getAttribute("aria-describedby")).toBe(message.id);
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	});
});

describe("Policy form round trip", () => {
	it("keeps hook arguments and exclude patterns exactly as stored", () => {
		const policy = makePolicy("nightly", {
			exclude: ["/tmp/ ", "**/*.swp"],
			hooks: {
				before: [{ script: "dump.sh", args: [" --flag ", "a  b"] }],
			},
		});
		const back = policyFromDraft(draftFromPolicy(policy));
		expect(back.spec.exclude).toEqual(["/tmp/ ", "**/*.swp"]);
		expect(back.spec.hooks?.before?.[0].args).toEqual([" --flag ", "a  b"]);
	});

	it("still drops blank lines and CRLF endings typed into the form", () => {
		const draft = draftFromPolicy(makePolicy("nightly"));
		const back = policyFromDraft({
			...draft,
			exclude: "a\r\n\r\n  \nb\r\n",
		});
		expect(back.spec.exclude).toEqual(["a", "b"]);
	});
});

describe("Hooks", () => {
	it("edits an inline command and reorders hooks", async () => {
		useSignedIn();
		const requests = mockValidate();
		mockPolicy(
			makeDetail(
				"nightly",
				{},
				{
					hooks: {
						before: [
							{ command: "pg_dump app", timeout: "30m" },
							{ script: "notify.sh" },
						],
					},
				},
			),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");

		expect(await screen.findByLabelText("Command")).toHaveValue("pg_dump app");
		await user.click(screen.getByRole("button", { name: "Move hook 1 down" }));
		await waitFor(() => {
			const before = requests.at(-1)?.policy?.spec.hooks?.before ?? [];
			expect(before.map((h) => h.script ?? h.command)).toEqual([
				"notify.sh",
				"pg_dump app",
			]);
		});
	});

	it("flags an empty new hook only after the field is left", async () => {
		mockValidate(() =>
			validResponse({
				valid: false,
				errors: [
					{
						path: "spec.hooks.before[0].script",
						message: "Enter a file name.",
					},
				],
			}),
		);
		const user = await openNew();
		await user.click(screen.getByRole("button", { name: "Hooks (optional)" }));
		await user.click(
			await screen.findByRole("button", { name: "Add before-backup hook" }),
		);
		const script = screen.getByLabelText("Script");
		await new Promise((r) => setTimeout(r, 400));
		expect(screen.queryByText("Enter a file name.")).not.toBeInTheDocument();
		await user.click(script);
		await user.tab();
		expect(await screen.findByText("Enter a file name.")).toBeInTheDocument();
	});

	it("switches a hook to an inline command", async () => {
		const requests = mockValidate();
		const user = await openNew();
		await user.click(screen.getByRole("button", { name: "Hooks (optional)" }));
		await user.click(
			await screen.findByRole("button", { name: "Add after-backup hook" }),
		);
		await user.click(screen.getByRole("radio", { name: "Command" }));
		await user.type(screen.getByLabelText("Command"), "echo done");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.hooks?.after).toEqual([
				{ command: "echo done" },
			]),
		);
	});

	it("picks hosts from a list instead of labels", async () => {
		const requests = mockValidate();
		server.use(
			http.get("/api/v1/hosts", () =>
				HttpResponse.json([
					{
						id: "h-1",
						hostname: "web-1",
						status: "online",
						repository_id: "repo-1",
						labels: [],
					},
					{
						id: "h-2",
						hostname: "db-1",
						status: "offline",
						repository_id: "repo-1",
						labels: [{ key: "role", value: "db" }],
					},
					{
						id: "h-3",
						hostname: "other",
						status: "online",
						repository_id: "repo-2",
						labels: [],
					},
				]),
			),
		);
		const user = await openNew();
		await user.click(screen.getByRole("radio", { name: "Pick hosts" }));
		expect(await screen.findByText("0 of 2 selected")).toBeInTheDocument();
		expect(screen.queryByText("other")).not.toBeInTheDocument();
		expect(screen.getByText("Pick at least one host.")).toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
		await user.click(screen.getByLabelText(/db-1/));
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.selector).toEqual({
				matchLabels: {},
				hosts: ["h-2"],
			}),
		);
	});

	it("adds a script hook from the form", async () => {
		const requests = mockValidate();
		const user = await openNew();
		await waitFor(() => expect(requests).toHaveLength(1));

		await user.click(screen.getByRole("button", { name: "Hooks (optional)" }));
		await user.click(
			await screen.findByRole("button", { name: "Add before-backup hook" }),
		);
		await user.type(screen.getByLabelText("Script"), "dump.sh");
		await user.type(screen.getByLabelText("Timeout"), "30m");

		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.hooks?.before).toEqual([
				{ script: "dump.sh", timeout: "30m" },
			]),
		);
	});
});

describe("Matching hosts", () => {
	it("caps the list at ten rows and expands on request", async () => {
		const hosts = Array.from({ length: 12 }, (_, i) =>
			matchHost(`host-${i + 1}`),
		);
		mockValidate(() => validResponse({}, hosts));
		const user = await openNew();

		expect(
			await screen.findByText("12 hosts will be backed up"),
		).toBeInTheDocument();
		const card = screen
			.getByRole("heading", { name: "Matching hosts" })
			.closest("[data-slot=card]") as HTMLElement;
		expect(within(card).getAllByRole("link")).toHaveLength(10);
		expect(
			screen.getByText(
				"No labels selected: every host in this repository matches.",
			),
		).toBeInTheDocument();

		await user.click(screen.getByRole("button", { name: "Show all 12 hosts" }));
		expect(within(card).getAllByRole("link")).toHaveLength(12);
	});

	it("groups hosts of another repository and shows hook warnings", async () => {
		mockValidate(() =>
			validResponse({
				matching: {
					hosts: [matchHost("web-1")],
					other_repository: [
						matchHost("old-1", { repository_name: "archive" }),
					],
				},
				warnings: [
					{
						host_id: "web-1-id",
						hostname: "web-1",
						kind: "script_missing",
						script: "dump.sh",
					},
					{ host_id: "web-1-id", hostname: "web-1", kind: "inline_disabled" },
				],
			}),
		);
		await openNew();

		expect(
			await screen.findByText("1 host will be backed up"),
		).toBeInTheDocument();
		expect(
			screen.getByText("1 use another repository and are skipped"),
		).toBeInTheDocument();
		expect(screen.getByText("Other repository")).toBeInTheDocument();
		expect(screen.getByText("Uses archive")).toBeInTheDocument();
		expect(
			screen.getByRole("img", {
				name: "Hook warning: Script dump.sh is not installed on this host",
			}),
		).toBeInTheDocument();
		expect(
			screen.getByRole("img", {
				name: "Hook warning: Inline commands are turned off on this host (hooks.allow_inline)",
			}),
		).toBeInTheDocument();
	});

	it("says when nothing matches yet", async () => {
		mockValidate(() => validResponse({}, []));
		await openNew();

		expect(
			await screen.findByText(
				"No hosts match yet. Check the labels, or add a label to a host.",
			),
		).toBeInTheDocument();
	});
});

describe("Unsaved changes", () => {
	it("asks before leaving, and discard restores the saved policy", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.type(first, "/x");
		expect(first).toHaveValue("/etc/x");
		await user.click(breadcrumbLink());

		const dialog = await screen.findByRole("alertdialog", {
			name: "Leave without saving?",
		});
		await user.click(
			within(dialog).getByRole("button", { name: "Keep editing" }),
		);
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
		expect(first).toHaveValue("/etc/x");

		await user.click(screen.getByRole("button", { name: "Discard changes" }));
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/etc",
		);
		expect(
			screen.queryByRole("button", { name: "Discard changes" }),
		).not.toBeInTheDocument();

		await user.click(breadcrumbLink());
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
	});

	it("keeps unsaved edits when the policy changes on the server, until reload", async () => {
		useSignedIn();
		mockValidate();
		let served = makeDetail("nightly");
		mockPolicy(served);
		server.use(
			http.get("/api/v1/policies/nightly", () => HttpResponse.json(served)),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		served = {
			...makeDetail("nightly", {}, { paths: ["/srv"] }),
			revision: 2,
		};
		window.dispatchEvent(new Event("visibilitychange"));

		expect(
			await screen.findByText(/This policy was changed elsewhere/),
		).toBeInTheDocument();
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/etc/x",
		);

		await user.click(screen.getByRole("button", { name: "Reload" }));
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/srv",
		);
		expect(
			screen.queryByText(/This policy was changed elsewhere/),
		).not.toBeInTheDocument();
	});

	it("adopts a newer server revision at once when nothing was edited", async () => {
		useSignedIn();
		mockValidate();
		let served = makeDetail("nightly");
		mockPolicy(served);
		server.use(
			http.get("/api/v1/policies/nightly", () => HttpResponse.json(served)),
		);
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		served = {
			...makeDetail("nightly", {}, { paths: ["/srv"] }),
			revision: 2,
		};
		window.dispatchEvent(new Event("visibilitychange"));

		await waitFor(() =>
			expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
				"/srv",
			),
		);
	});

	it("sends the base revision and shows the reload banner on a conflict, keeping the edits", async () => {
		useSignedIn();
		mockValidate();
		let served = makeDetail("nightly");
		mockPolicy(served);
		const bodies: unknown[] = [];
		server.use(
			http.get("/api/v1/policies/nightly", () => HttpResponse.json(served)),
			http.put("/api/v1/policies/nightly", async ({ request }) => {
				bodies.push(await request.json());
				served = {
					...makeDetail("nightly", {}, { paths: ["/srv"] }),
					revision: 2,
				};
				return HttpResponse.json(
					{ code: "policy_conflict", message: "changed elsewhere" },
					{ status: 409 },
				);
			}),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		await waitFor(() =>
			expect(screen.getByRole("button", { name: "Save policy" })).toBeEnabled(),
		);
		await user.click(screen.getByRole("button", { name: "Save policy" }));

		expect(
			await screen.findByText(/This policy was changed elsewhere/),
		).toBeInTheDocument();
		expect(bodies).toHaveLength(1);
		expect(bodies[0]).toMatchObject({ base_revision: 1 });
		expect(screen.queryByText(/Could not save/i)).not.toBeInTheDocument();
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/etc/x",
		);

		await user.click(await screen.findByRole("button", { name: "Reload" }));
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/srv",
		);
	});

	it("treats the saved policy as the new baseline", async () => {
		useSignedIn();
		mockValidate();
		let served = makeDetail("nightly");
		mockPolicy(served);
		server.use(
			http.get("/api/v1/policies/nightly", () => HttpResponse.json(served)),
			http.put("/api/v1/policies/nightly", () => {
				served = {
					...makeDetail("nightly", {}, { paths: ["/etc/x", "/home"] }),
					revision: 2,
				};
				return HttpResponse.json(served);
			}),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		await waitFor(() =>
			expect(screen.getByRole("button", { name: "Save policy" })).toBeEnabled(),
		);
		await user.click(screen.getByRole("button", { name: "Save policy" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("button", { name: "Discard changes" }),
			).not.toBeInTheDocument(),
		);
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toHaveValue(
			"/etc/x",
		);
	});

	it("does not ask to save a policy that was just deleted", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		mockPolicies([]);
		server.use(
			http.delete(
				"/api/v1/policies/nightly",
				() => new HttpResponse(null, { status: 204 }),
			),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		await user.click(screen.getByRole("button", { name: "More actions" }));
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Delete policy" }),
		);

		expect(await screen.findByText("No policies yet")).toBeInTheDocument();
		expect(screen.queryByText("Leave without saving?")).not.toBeInTheDocument();
	});

	it("leaves when confirmed", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		await user.click(breadcrumbLink());
		await user.click(
			await screen.findByRole("button", { name: "Leave without saving" }),
		);

		await waitFor(() =>
			expect(
				screen.queryByRole("heading", { name: "nightly" }),
			).not.toBeInTheDocument(),
		);
	});
});

function summary(
	name: string,
	over: Partial<PolicySummary> = {},
): PolicySummary {
	return {
		name,
		repository_id: "repo-1",
		repository_name: "main",
		match_labels: [],
		host_list: false,
		hosts: 2,
		other_repository_hosts: 0,
		last_job: null,
		...over,
	};
}

function mockPolicies(list: PolicySummary[]) {
	server.use(http.get("/api/v1/policies", () => HttpResponse.json(list)));
}

async function openList() {
	useSignedIn();
	const user = userEvent.setup();
	renderApp("/policies");
	await screen.findByRole("heading", { name: "Policies" });
	return user;
}

describe("Policies list", () => {
	it("shows selectors, host counts and the last run", async () => {
		mockPolicies([
			summary("etc-and-home", {
				match_labels: [
					{ key: "env", value: "prod" },
					{ key: "role", value: "db" },
					{ key: "tier", value: "1" },
				],
				other_repository_hosts: 1,
				last_job: {
					id: "j1",
					state: "succeeded",
					queued_at: Date.now() - 60_000,
				},
			}),
			summary("nightly"),
		]);
		await openList();

		await screen.findByRole("link", { name: "Open etc-and-home" });
		const rows = within(screen.getByRole("table")).getAllByRole("row");
		expect(rows).toHaveLength(3);
		const first = within(rows[1]);
		expect(
			first.getByRole("link", { name: "Open etc-and-home" }),
		).toHaveAttribute("href", "/policies/etc-and-home");
		expect(first.getByText("env=prod")).toBeInTheDocument();
		expect(first.getByText("role=db")).toBeInTheDocument();
		expect(first.queryByText("tier=1")).not.toBeInTheDocument();
		expect(first.getByText("+1")).toBeInTheDocument();
		expect(first.getByText("+1 in other repositories")).toBeInTheDocument();
		expect(
			first.getByText("Succeeded").closest("[data-status]"),
		).toHaveAttribute("data-status", "succeeded");
		const second = within(rows[2]);
		expect(second.getByText("All hosts")).toBeInTheDocument();
		expect(second.getByText("Never run")).toBeInTheDocument();
	});

	it("invites creating the first policy", async () => {
		mockPolicies([]);
		await openList();

		expect(await screen.findByText("No policies yet")).toBeInTheDocument();
		expect(screen.getAllByRole("link", { name: "Create policy" })).toHaveLength(
			2,
		);
	});

	it("points to adding a repository when there is none", async () => {
		const root = createRootRoute({
			component: () => (
				<ThemeProvider>
					<PoliciesTable
						policies={[]}
						isPending={false}
						isError={false}
						onRetry={() => {}}
						hasRepositories={false}
						onRun={() => {}}
						onDelete={() => {}}
					/>
				</ThemeProvider>
			),
		});
		const router = createRouter({
			routeTree: root.addChildren([
				createRoute({
					getParentRoute: () => root,
					path: "/settings/repositories/new",
				}),
			]),
			history: createMemoryHistory({ initialEntries: ["/"] }),
		});
		render(<RouterProvider router={router} />);

		expect(
			await screen.findByText(
				"Policies back up into a repository. Add one first.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Add repository" }),
		).toHaveAttribute("href", "/settings/repositories/new");
	});

	it("disables Run now with no matching hosts", async () => {
		mockPolicies([summary("lonely", { hosts: 0 })]);
		const user = await openList();

		await user.click(
			await screen.findByRole("button", { name: "Actions for lonely" }),
		);
		expect(
			await screen.findByRole("menuitem", { name: "Run now" }),
		).toHaveAttribute("aria-disabled", "true");
		expect(screen.getByRole("menuitem", { name: "Edit" })).toBeInTheDocument();
		expect(
			screen.getByRole("menuitem", { name: "Delete policy" }),
		).toBeInTheDocument();
	});

	it("runs a policy from the menu and links to the existing job when merged", async () => {
		mockPolicies([summary("nightly")]);
		mockPolicy(makeDetail("nightly"));
		server.use(
			http.post("/api/v1/policies/nightly/run", () =>
				HttpResponse.json({
					jobs: [
						{ id: "job-9", host_id: "a", hostname: "web-1", created: false },
					],
				}),
			),
		);
		const user = await openList();

		await user.click(
			await screen.findByRole("button", { name: "Actions for nightly" }),
		);
		await user.click(await screen.findByRole("menuitem", { name: "Run now" }));
		const dialog = await screen.findByRole("dialog", {
			name: 'Run "nightly" now?',
		});
		expect(await within(dialog).findByText("web-1")).toBeInTheDocument();
		await user.click(
			within(dialog).getByRole("button", { name: "Run on 2 hosts" }),
		);

		expect(
			await screen.findByText(
				"Already queued for web-1. Showing the existing job.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "View job" }),
		).toBeInTheDocument();
	});

	it("deletes a policy after confirmation", async () => {
		mockPolicies([summary("nightly")]);
		const deleted: string[] = [];
		server.use(
			http.delete("/api/v1/policies/nightly", () => {
				deleted.push("nightly");
				return new HttpResponse(null, { status: 204 });
			}),
		);
		const user = await openList();

		await user.click(
			await screen.findByRole("button", { name: "Actions for nightly" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: 'Delete policy "nightly"?',
		});
		await user.click(
			within(dialog).getByRole("button", { name: "Delete policy" }),
		);

		expect(
			await screen.findByText('Policy "nightly" deleted.'),
		).toBeInTheDocument();
		expect(deleted).toEqual(["nightly"]);
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
	});

	it("keeps the dialog open and shows the server message when deletion is blocked", async () => {
		mockPolicies([summary("nightly")]);
		const message =
			'Policy "nightly" has 2 jobs in progress. Cancel them first, then delete the policy.';
		server.use(
			http.delete("/api/v1/policies/nightly", () =>
				apiError(409, "conflict", message),
			),
		);
		const user = await openList();

		await user.click(
			await screen.findByRole("button", { name: "Actions for nightly" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Delete policy" }),
		);

		expect(await within(dialog).findByText(message)).toBeInTheDocument();
		expect(screen.getByRole("alertdialog")).toBeInTheDocument();
	});
});

describe("Recent jobs", () => {
	it("lists the five latest jobs of the policy, each linking to its job", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		const seen: string[] = [];
		const job = (n: number): JobSummary => ({
			id: `job-${n}`,
			kind: "backup",
			host_id: `host-${n}`,
			hostname: `web-${n}`,
			host_online: true,
			policy_name: "nightly",
			policy_exists: true,
			source: "manual",
			state: "succeeded",
			queued_at: Date.now() - n * 60_000,
			finished_at: Date.now() - n * 60_000 + 5_000,
			incomplete: false,
			warnings_total: 0,
			errors_total: 0,
		});
		server.use(
			http.get("/api/v1/jobs", ({ request }) => {
				seen.push(new URL(request.url).search);
				return HttpResponse.json({
					items: [1, 2, 3, 4, 5, 6].map(job),
					page: 1,
					page_size: 50,
					total: 6,
				});
			}),
		);
		renderApp("/policies/nightly");

		const heading = await screen.findByRole("heading", { name: "Recent jobs" });
		const card = heading.closest("[data-slot=card]") as HTMLElement;
		await within(card).findByText("web-1");
		expect(within(card).getAllByRole("listitem")).toHaveLength(5);
		expect(within(card).getByRole("link", { name: "web-1" })).toHaveAttribute(
			"href",
			"/jobs/job-1",
		);
		expect(within(card).queryByText("web-6")).not.toBeInTheDocument();
		expect(within(card).queryByText("Policy nightly")).not.toBeInTheDocument();
		expect(
			within(card).getByRole("link", { name: "View all jobs" }),
		).toHaveAttribute("href", "/jobs?policy=nightly");
		expect(seen[0]).toContain("policy=nightly");
	});

	it("reruns and cancels jobs instead of only opening them", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		const base = {
			host_id: "host-1",
			hostname: "web-1",
			host_online: true,
			policy_name: "nightly",
			policy_exists: true,
			source: "manual" as const,
			queued_at: Date.now() - 60_000,
			incomplete: false,
			warnings_total: 0,
			errors_total: 0,
		};
		const reruns: string[] = [];
		const cancels: string[] = [];
		server.use(
			http.get("/api/v1/jobs", () =>
				HttpResponse.json({
					items: [
						{ ...base, id: "run-1", state: "running" },
						{ ...base, id: "done-1", state: "succeeded" },
					],
					page: 1,
					page_size: 50,
					total: 2,
				}),
			),
			http.post("/api/v1/jobs/:id/rerun", ({ params }) => {
				reruns.push(String(params.id));
				return HttpResponse.json({
					id: "new-1",
					host_id: "host-1",
					hostname: "web-1",
					created: true,
				});
			}),
			http.post("/api/v1/jobs/:id/cancel", ({ params }) => {
				cancels.push(String(params.id));
				return HttpResponse.json({ ...base, id: "run-1", state: "cancelled" });
			}),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		const heading = await screen.findByRole("heading", { name: "Recent jobs" });
		const card = heading.closest("[data-slot=card]") as HTMLElement;
		const actions = await within(card).findAllByRole("button", {
			name: "Actions for web-1",
		});

		await user.click(actions[1]);
		await user.click(
			await screen.findByRole("menuitem", { name: "Run again" }),
		);
		await waitFor(() => expect(reruns).toEqual(["done-1"]));

		await user.click(actions[0]);
		await user.click(
			await screen.findByRole("menuitem", { name: "Cancel job" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Cancel job" }),
		);
		await waitFor(() => expect(cancels).toEqual(["run-1"]));
		expect(
			screen.getByRole("heading", { name: "nightly" }),
		).toBeInTheDocument();
	});

	it("refreshes the list after Run now on the policy page", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		let listed = 0;
		server.use(
			http.get("/api/v1/jobs", () => {
				listed += 1;
				return HttpResponse.json({
					items: [],
					page: 1,
					page_size: 50,
					total: 0,
				});
			}),
			http.post("/api/v1/policies/nightly/run", () =>
				HttpResponse.json({
					jobs: [
						{ id: "job-1", host_id: "a", hostname: "web-1", created: true },
					],
				}),
			),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });
		await waitFor(() => expect(listed).toBe(1));

		await user.click(screen.getByRole("button", { name: "Run now" }));
		const dialog = await screen.findByRole("dialog");
		await user.click(
			await within(dialog).findByRole("button", { name: "Run on 2 hosts" }),
		);

		await waitFor(() => expect(listed).toBe(2));
	});

	it("deletes the policy from its page and returns to the list", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		mockPolicies([]);
		server.use(
			http.delete(
				"/api/v1/policies/nightly",
				() => new HttpResponse(null, { status: 204 }),
			),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.click(screen.getByRole("button", { name: "More actions" }));
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await user.click(
			within(dialog).getByRole("button", { name: "Delete policy" }),
		);

		expect(await screen.findByText("No policies yet")).toBeInTheDocument();
	});
});
