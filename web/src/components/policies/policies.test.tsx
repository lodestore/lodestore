import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
	createMemoryHistory,
	createRootRoute,
	createRoute,
	createRouter,
	RouterProvider,
} from "@tanstack/react-router";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { delay, HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	BackupPolicy,
	HostDetail,
	HostRetentionPreview,
	JobSummary,
	MatchHost,
	PolicyDetail,
	PolicySummary,
	RetentionPreview,
	ScheduleInfo,
	ValidateRequest,
	ValidateResponse,
} from "@/api/gen/model";
import { HostPolicies } from "@/components/hosts/host-policies";
import { PoliciesTable } from "@/components/policies/policies-table";
import {
	draftFromPolicy,
	policyFromDraft,
} from "@/components/policies/policy-form";
import { ThemeProvider } from "@/components/theme/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { formatDateTime } from "@/lib/format";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

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

async function openOptions(user: ReturnType<typeof userEvent.setup>) {
	await user.click(await screen.findByRole("button", { name: /^Options · / }));
	return screen.findByRole("textbox", {
		name: "Alert if no backup succeeds for",
	});
}

describe("Dead-man threshold", () => {
	it("saves the dead-man threshold", async () => {
		const requests = mockValidate();
		const created: unknown[] = [];
		server.use(
			http.post("/api/v1/policies", async ({ request }) => {
				created.push(await request.json());
				return HttpResponse.json(makeDetail("etc-and-home"), { status: 201 });
			}),
			http.get("/api/v1/policies/etc-and-home", () =>
				HttpResponse.json(makeDetail("etc-and-home")),
			),
		);
		const user = await openNew();
		const field = await openOptions(user);
		expect(field).toHaveAttribute("placeholder", "auto (2 intervals + jitter)");
		expect(field).toHaveValue("");
		expect(
			screen.getByText(
				"Leave empty to alert after two missed runs. Policies without a schedule never alert.",
			),
		).toBeInTheDocument();

		await user.type(field, "36h");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.alerts).toEqual({ maxAge: "36h" }),
		);
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);
		await waitFor(() => expect(created).toHaveLength(1));
		expect(created[0]).toMatchObject({
			policy: { spec: { alerts: { maxAge: "36h" } } },
		});
	});

	it("omits empty alerts", async () => {
		const requests = mockValidate();
		const user = await openNew();
		await waitFor(() => expect(requests.length).toBeGreaterThan(0));
		const field = await openOptions(user);
		await user.type(field, "36h");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.alerts).toEqual({ maxAge: "36h" }),
		);
		await user.clear(field);
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec).not.toHaveProperty("alerts"),
		);
		expect(
			policyFromDraft(draftFromPolicy(makePolicy("nightly"))).spec.alerts,
		).toBeUndefined();
	});

	it("shows the threshold error", async () => {
		mockValidate((body) =>
			body.policy?.spec.alerts?.maxAge === "soon"
				? {
						valid: false,
						errors: [
							{
								path: "spec.alerts.maxAge",
								message: "Use a duration such as 36h or 3d.",
							},
						],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		const field = await openOptions(user);
		await user.type(field, "soon");
		const message = await screen.findByText(
			"Use a duration such as 36h or 3d.",
		);
		await waitFor(() => expect(field).toHaveAttribute("aria-invalid", "true"));
		expect(field.getAttribute("aria-describedby")).toContain(message.id);
	});
});

const pathsValues = () =>
	screen
		.getAllByRole("textbox", { name: "Paths" })
		.map((el) => (el as HTMLInputElement).value);

describe("Split policy editor", () => {
	it("split editor shows both panes", async () => {
		mockValidate();
		await openNew();

		expect(screen.getByRole("group", { name: "Policy form" })).toBeVisible();
		const pane = screen.getByRole("group", { name: "Policy YAML" });
		expect(within(pane).getByRole("heading", { name: "YAML" })).toBeVisible();
		expect(
			within(pane).getByRole("textbox", { name: "Policy YAML" }),
		).toBeVisible();
		expect(pathsValues()).toEqual(["/etc", "/home"]);
	});

	it("form edit rewrites yaml after debounce", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `paths: ${body.policy?.spec.paths.join(",")}\n`,
			}),
		);
		const user = await openNew();
		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(yaml).toHaveValue("paths: /etc,/home\n"));

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "/srv");

		await waitFor(() => expect(yaml).toHaveValue("paths: /srv,/home\n"));
		expect(requests.at(-1)?.policy?.spec.paths).toEqual(["/srv", "/home"]);
	}, 15_000);

	it("yaml edit updates form fields", async () => {
		mockValidate((body) =>
			body.yaml
				? validResponse({
						policy: makePolicy("etc-and-home", { paths: ["/srv"] }),
					})
				: validResponse(),
		);
		const user = await openNew();
		const field = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(field).not.toHaveValue(""));
		await user.clear(field);
		await user.click(field);
		await user.paste("kind: BackupPolicy\n");

		await waitFor(() => expect(pathsValues()).toEqual(["/srv"]));
		expect(field).toHaveValue("kind: BackupPolicy\n");
	});

	it("sync status announces state", async () => {
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
		const pane = screen.getByRole("group", { name: "Policy YAML" });
		const status = within(pane).getByRole("status");
		expect(status).toHaveAttribute("aria-live", "polite");
		await waitFor(() => expect(status).toHaveTextContent("In sync"));

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "var");
		expect(status).toHaveTextContent("Updating…");
		await waitFor(() => expect(status).toHaveTextContent("1 error"));

		await user.clear(first);
		await user.type(first, "/ok");
		await waitFor(() => expect(status).toHaveTextContent("In sync"));
	}, 15_000);
});

const invalidYaml = (body: ValidateRequest): ValidateResponse | undefined =>
	body.yaml?.includes("schedule")
		? {
				valid: false,
				errors: [{ line: 4, message: "unknown field `schedule`" }],
				name_taken: false,
				warnings: [],
			}
		: undefined;

describe("Split editor states", () => {
	it("caret rule keeps yaml comments", async () => {
		mockValidate((body) =>
			body.yaml
				? validResponse({
						yaml: "rewritten: true\n",
						policy: makePolicy("etc-and-home", { paths: ["/srv"] }),
					})
				: validResponse(),
		);
		const user = await openNew();
		const field = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(field).not.toHaveValue(""));
		await user.clear(field);
		await user.click(field);
		await user.paste("# keep\nkind: BackupPolicy\n");

		await waitFor(() => expect(pathsValues()).toEqual(["/srv"]));
		expect(field).toHaveValue("# keep\nkind: BackupPolicy\n");
	});

	it("invalid yaml locks form", async () => {
		mockValidate((body) => invalidYaml(body) ?? validResponse());
		const user = await openNew();
		const field = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(field).not.toHaveValue(""));
		await user.clear(field);
		await user.click(field);
		await user.paste("spec:\n  schedule: daily\n");

		expect(
			await screen.findByText(
				"The YAML has errors. The form shows the last valid version and is read-only until you fix them.",
			),
		).toBeInTheDocument();
		expect(screen.getAllByRole("textbox", { name: "Paths" })[0]).toBeDisabled();
		const pane = screen.getByRole("group", { name: "Policy YAML" });
		expect(within(pane).getByRole("status")).toHaveTextContent("1 error");
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	});

	it("invalid form keeps last valid yaml", async () => {
		mockValidate((body) =>
			body.policy?.spec.paths.some((p) => !p.startsWith("/"))
				? {
						valid: false,
						errors: [{ path: "spec.paths[0]", message: "Bad path." }],
						name_taken: false,
						warnings: [],
					}
				: validResponse({ yaml: "last: valid\n" }),
		);
		const user = await openNew();
		const field = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(field).toHaveValue("last: valid\n"));

		const [first] = screen.getAllByRole("textbox", { name: "Paths" });
		await user.clear(first);
		await user.type(first, "var");

		expect(
			await screen.findByText(
				"Showing the last valid YAML. Fix the errors in the form to update it.",
			),
		).toBeInTheDocument();
		expect(field).toHaveValue("last: valid\n");
		expect(first).toBeEnabled();
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	}, 15_000);

	it("narrow mode refuses form tab", async () => {
		mockValidate((body) => invalidYaml(body) ?? validResponse());
		const user = await openNew();
		const field = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(field).not.toHaveValue(""));
		await user.click(screen.getByRole("tab", { name: "YAML" }));
		await user.clear(field);
		await user.click(field);
		await user.paste("spec:\n  schedule: daily\n");
		await screen.findByText("Line 4: unknown field `schedule`");
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
	});

	it("footer reflects dirty state", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly"));
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		const save = screen.getByRole("button", { name: "Save policy" });
		const run = screen.getByRole("button", { name: "Run now" });
		expect(save).toHaveAttribute("data-variant", "outline");
		expect(save).toBeDisabled();
		expect(run).toHaveAttribute("data-variant", "default");
		expect(screen.getByText("All changes saved")).toBeInTheDocument();
		expect(
			screen.queryByRole("button", { name: "Discard changes" }),
		).not.toBeInTheDocument();

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");

		expect(screen.getByText("Unsaved changes")).toBeInTheDocument();
		expect(run).toHaveAttribute("data-variant", "outline");
		expect(save).toHaveAttribute("data-variant", "default");
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(screen.getByRole("button", { name: "Discard changes" }));

		expect(screen.getByText("All changes saved")).toBeInTheDocument();
		expect(save).toBeDisabled();
		expect(run).toHaveAttribute("data-variant", "default");
	}, 15_000);
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

	it("keeps empty and multi-line hook arguments the form cannot show", () => {
		const args = ["--password", "", " ", "a\nb"];
		const policy = makePolicy("nightly", {
			hooks: { before: [{ script: "dump.sh", args }] },
		});
		const draft = draftFromPolicy(policy);
		expect(draft.before[0].rawArgs).toEqual(args);
		expect(policyFromDraft(draft).spec.hooks?.before?.[0].args).toEqual(args);
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
		const requests = mockValidate(() =>
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
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.hooks?.before).toHaveLength(1),
		);
		await act(async () => {});
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

	it("lists ten picked-host rows and shows the rest on request", async () => {
		mockValidate();
		server.use(
			http.get("/api/v1/hosts", () =>
				HttpResponse.json(
					Array.from({ length: 12 }, (_, i) => ({
						id: `h-${i + 1}`,
						hostname: `node-${i + 1}`,
						status: "online",
						repository_id: "repo-1",
						labels: [],
					})),
				),
			),
		);
		const user = await openNew();
		await user.click(screen.getByRole("radio", { name: "Pick hosts" }));
		expect(await screen.findByText("0 of 12 selected")).toBeInTheDocument();
		expect(screen.getAllByRole("checkbox", { name: /^node-/ })).toHaveLength(
			10,
		);
		await user.click(screen.getByRole("button", { name: "Show all 12 hosts" }));
		expect(screen.getAllByRole("checkbox", { name: /^node-/ })).toHaveLength(
			12,
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
		expect(within(card).queryAllByRole("link")).toHaveLength(0);
		await user.click(screen.getByRole("button", { name: "Show hosts" }));
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
		const user = await openNew();

		expect(
			await screen.findByText(
				"1 host will be backed up · 1 use another repository and are skipped",
			),
		).toBeInTheDocument();
		expect(screen.getByText("2 hook warnings")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Show hosts" }));
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

	it("matching strip toggles hosts", async () => {
		mockValidate();
		const user = await openNew();

		await screen.findByText("2 hosts will be backed up");
		const toggle = screen.getByRole("button", { name: "Show hosts" });
		expect(toggle).toHaveAttribute("aria-expanded", "false");
		expect(
			screen.queryByRole("link", { name: "web-1" }),
		).not.toBeInTheDocument();

		await user.click(toggle);
		expect(screen.getByRole("link", { name: "web-1" })).toBeInTheDocument();
		const hide = screen.getByRole("button", { name: "Hide hosts" });
		expect(hide).toHaveAttribute("aria-expanded", "true");

		await user.click(hide);
		expect(
			screen.queryByRole("link", { name: "web-1" }),
		).not.toBeInTheDocument();
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
			origin: "manual",
			snapshot_suspicious: false,
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

const kyivSchedule: ScheduleInfo = {
	summary: "Every day at 03:30",
	timezone: "Europe/Kyiv",
	jitter_secs: 900,
	catch_up_secs: 86_400,
	next_runs: [
		Date.UTC(2026, 9, 20, 0, 30),
		Date.UTC(2026, 9, 21, 0, 30),
		Date.UTC(2026, 9, 22, 0, 30),
	],
	dst_notes: [
		{ date: "2026-10-25", time: "03:30", kind: "repeated", runs_at: null },
		{ date: "2027-03-28", time: "03:30", kind: "skipped", runs_at: "04:00" },
	],
};

const scheduled = (body: ValidateRequest) =>
	body.policy?.spec.schedule?.cron
		? validResponse({ schedule: kyivSchedule })
		: validResponse();

describe("Schedule section", () => {
	it("schedule section shows next runs and dst note", async () => {
		mockValidate(scheduled);
		const user = await openNew();

		expect(screen.queryByText("Next runs")).not.toBeInTheDocument();
		const cron = screen.getByRole("textbox", { name: "Cron" });
		expect(cron).toHaveAttribute("placeholder", "30 2 * * *");
		expect(screen.getByRole("combobox", { name: /Time zone/ })).toBeDisabled();
		await user.type(cron, "30 3 * * *");

		const block = (await screen.findByText("Next runs")).parentElement;
		expect(block).toHaveAttribute("aria-live", "polite");
		const next = within(block as HTMLElement);
		expect(next.getByText("Every day at 03:30")).toBeInTheDocument();
		expect(next.getByText("Tue 20 Oct, 03:30")).toBeInTheDocument();
		expect(next.getByText("Wed 21 Oct, 03:30")).toBeInTheDocument();
		expect(next.getByText("Thu 22 Oct, 03:30")).toBeInTheDocument();
		expect(next.getByText("Each host starts up to 15m later.")).toBeVisible();
		expect(
			next.getByText(
				"Sun 25 Oct: 03:30 happens twice that day, so the backup runs only the first time.",
			),
		).toBeVisible();
		expect(
			next.getByText(
				"Sun 28 Mar: 03:30 does not exist that day, so the backup runs once at 04:00.",
			),
		).toBeVisible();
	}, 15_000);

	it("hides next runs and shows the cron error under the field", async () => {
		mockValidate((body) =>
			body.policy?.spec.schedule?.cron === "nope"
				? {
						valid: false,
						errors: [
							{
								path: "spec.schedule.cron",
								message:
									"Use five fields (minute hour day month weekday) or @hourly, @daily, @weekly, @monthly.",
							},
						],
						name_taken: false,
						warnings: [],
					}
				: scheduled(body),
		);
		const user = await openNew();
		const cron = screen.getByRole("textbox", { name: "Cron" });
		await user.type(cron, "30 3 * * *");
		await screen.findByText("Next runs");
		await user.clear(cron);
		await user.type(cron, "nope");

		await waitFor(() => expect(cron).toHaveAttribute("aria-invalid", "true"));
		const described = cron.getAttribute("aria-describedby") ?? "";
		const message = (await screen.findAllByText(/Use five fields/)).find((el) =>
			described.split(" ").includes(el.id),
		);
		expect(message).toBeDefined();
		expect(screen.queryByText("Next runs")).not.toBeInTheDocument();
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	}, 15_000);

	it("schedule cron edit updates yaml", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `cron: ${body.policy?.spec.schedule?.cron ?? "none"}\n`,
				...(body.yaml && {
					policy: makePolicy("etc-and-home", {
						schedule: { cron: "0 4 * * *", timezone: "Europe/Kyiv" },
					}),
				}),
			}),
		);
		const user = await openNew();
		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await user.type(
			screen.getByRole("textbox", { name: "Cron" }),
			"30 2 * * *",
		);

		await waitFor(() => expect(yaml).toHaveValue("cron: 30 2 * * *\n"));
		expect(requests.at(-1)?.policy?.spec.schedule).toEqual({
			cron: "30 2 * * *",
		});

		await user.clear(yaml);
		await user.click(yaml);
		await user.paste("kind: BackupPolicy\n");
		await waitFor(() =>
			expect(screen.getByRole("textbox", { name: "Cron" })).toHaveValue(
				"0 4 * * *",
			),
		);
		expect(
			screen.getByRole("combobox", { name: "Time zone: Europe/Kyiv" }),
		).toBeEnabled();
	}, 15_000);

	it("timezone picker filters and selects", async () => {
		const requests = mockValidate(scheduled);
		const user = await openNew();
		await user.type(
			screen.getByRole("textbox", { name: "Cron" }),
			"30 3 * * *",
		);

		const picker = screen.getByRole("combobox", { name: "Time zone: UTC" });
		expect(picker).toBeEnabled();
		await user.click(picker);
		expect(picker).toHaveAttribute("aria-expanded", "true");
		const search = await screen.findByRole("combobox", {
			name: "Search time zones",
		});
		await waitFor(() => expect(search).toHaveFocus());
		await user.type(search, "kyiv");

		expect(
			await screen.findByRole("option", { name: "Europe/Kyiv" }),
		).toBeVisible();
		expect(
			screen.queryByRole("option", { name: "Asia/Tokyo" }),
		).not.toBeInTheDocument();
		await user.click(screen.getByRole("option", { name: "Europe/Kyiv" }));

		await waitFor(() =>
			expect(
				screen.getByRole("combobox", { name: "Time zone: Europe/Kyiv" }),
			).toBeInTheDocument(),
		);
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.schedule).toEqual({
				cron: "30 3 * * *",
				timezone: "Europe/Kyiv",
			}),
		);
	}, 15_000);

	it("options header shows jitter and catch up and opens on changed values", async () => {
		mockValidate();
		const user = await openNew();
		const trigger = screen.getByRole("button", {
			name: "Options · jitter 15m · catch up 24h",
		});
		expect(trigger).toHaveAttribute("aria-expanded", "false");
		await user.click(trigger);
		expect(screen.getByRole("textbox", { name: "Jitter" })).toBeDisabled();
		expect(
			screen.getByText("Set a cron expression to use these options."),
		).toBeVisible();
		await user.type(
			screen.getByRole("textbox", { name: "Cron" }),
			"30 2 * * *",
		);
		await user.type(screen.getByRole("textbox", { name: "Jitter" }), "5m");
		expect(
			screen.getByRole("button", {
				name: "Options · jitter 5m · catch up 24h",
			}),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"Catch up runs one backup after missed slots, if the last missed slot is within the window.",
			),
		).toBeVisible();
	});
});

describe("Policies list schedule", () => {
	it("policies table shows schedule column", async () => {
		const soon = Date.now() + 3 * 3_600_000 + 60_000;
		mockPolicies([
			summary("nightly", {
				schedule: {
					...kyivSchedule,
					next_runs: [soon],
				},
			}),
			summary("by-hand"),
		]);
		await openList();

		await screen.findByRole("link", { name: "Open nightly" });
		const table = within(screen.getByRole("table"));
		expect(
			table.getByRole("columnheader", { name: "Schedule" }),
		).toBeInTheDocument();
		const rows = table.getAllByRole("row");
		const first = within(rows[1]);
		expect(first.getByText("Every day at 03:30")).toBeInTheDocument();
		expect(first.getByText("in 3 hours")).toBeInTheDocument();
		expect(within(rows[2]).getByText("Manual only")).toBeInTheDocument();
	});
});

describe("Limits section", () => {
	const limited = {
		uploadRate: "50MiB/s",
		nice: 10,
		ioniceClass: "idle",
	} as const;

	it("limits section summary header", async () => {
		mockValidate();
		await openNew();
		const closed = screen.getByRole("button", { name: "Limits · no limits" });
		expect(closed).toHaveAttribute("aria-expanded", "false");
	});

	it("limits section opens with the values of a limited policy", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("nightly", {}, { limits: limited }));
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		const header = screen.getByRole("button", {
			name: "Limits · upload 50MiB/s · nice 10 · I/O idle",
		});
		expect(header).toHaveAttribute("aria-expanded", "true");
		expect(screen.getByRole("textbox", { name: "Upload rate" })).toHaveValue(
			"50MiB/s",
		);
		expect(
			screen.getByRole("textbox", { name: "CPU priority (nice)" }),
		).toHaveValue("10");
		expect(
			screen.getByText(
				"Idle uses the disk only when nothing else does. Backups can take much longer.",
			),
		).toBeVisible();
	});

	it("limits io class toggles level", async () => {
		const requests = mockValidate();
		const user = await openNew();
		await user.click(
			screen.getByRole("button", { name: "Limits · no limits" }),
		);

		const io = screen.getByRole("combobox", { name: "I/O priority" });
		expect(io).toHaveTextContent("Default");
		expect(
			screen.queryByRole("textbox", { name: "Level" }),
		).not.toBeInTheDocument();

		await user.click(io);
		await user.click(
			await screen.findByRole("option", { name: "Best effort" }),
		);
		const level = screen.getByRole("textbox", { name: "Level" });
		expect(level).toHaveAttribute("placeholder", "4");
		expect(screen.getByText("0 is the highest priority.")).toBeVisible();
		await user.type(level, "2");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.limits).toEqual({
				ioniceClass: "best-effort",
				ioniceLevel: 2,
			}),
		);

		await user.click(screen.getByRole("combobox", { name: "I/O priority" }));
		await user.click(await screen.findByRole("option", { name: "Idle" }));
		expect(
			screen.queryByRole("textbox", { name: "Level" }),
		).not.toBeInTheDocument();
		expect(
			screen.getByText(
				"Idle uses the disk only when nothing else does. Backups can take much longer.",
			),
		).toBeVisible();

		await user.click(screen.getByRole("combobox", { name: "I/O priority" }));
		await user.click(await screen.findByRole("option", { name: "Default" }));
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.limits).toBeUndefined(),
		);
	}, 20_000);

	it("limits field error shown", async () => {
		mockValidate((body) =>
			(body.policy?.spec.limits?.nice ?? 0) > 19
				? {
						valid: false,
						errors: [
							{
								path: "spec.limits.nice",
								message: "Enter a whole number from 0 to 19.",
							},
						],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		await user.click(
			screen.getByRole("button", { name: "Limits · no limits" }),
		);
		const nice = screen.getByRole("textbox", { name: "CPU priority (nice)" });
		await user.type(nice, "25");

		const message = await screen.findByText(
			"Enter a whole number from 0 to 19.",
			{
				selector: "p[id]",
			},
		);
		await waitFor(() => expect(nice).toHaveAttribute("aria-invalid", "true"));
		expect(nice.getAttribute("aria-describedby")).toContain(message.id);
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	}, 15_000);

	it("limits max run time round trips", async () => {
		useSignedIn();
		const requests = mockValidate();
		mockPolicy(makeDetail("nightly", {}, { limits: { maxRuntime: "30h" } }));
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		expect(
			screen.getByRole("button", { name: "Limits · max run 30h" }),
		).toHaveAttribute("aria-expanded", "true");
		const field = screen.getByRole("textbox", { name: "Max run time" });
		expect(field).toHaveValue("30h");
		expect(field).toHaveAttribute("placeholder", "12h");
		expect(
			screen.getByText(
				"A backup that runs longer stops and saves no snapshot.",
			),
		).toBeVisible();

		const user = userEvent.setup();
		await user.type(
			screen.getByRole("textbox", { name: "Upload rate" }),
			"20MiB/s",
		);
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.limits).toEqual({
				uploadRate: "20MiB/s",
				maxRuntime: "30h",
			}),
		);

		await user.clear(field);
		await user.clear(screen.getByRole("textbox", { name: "Upload rate" }));
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.limits).toBeUndefined(),
		);
	}, 15_000);

	it("limits max run time error under field", async () => {
		mockValidate((body) =>
			body.policy?.spec.limits?.maxRuntime === "90m"
				? {
						valid: false,
						errors: [
							{
								path: "spec.limits.maxRuntime",
								message: "Enter a time from 1h to 7d.",
							},
						],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		await user.click(
			screen.getByRole("button", { name: "Limits · no limits" }),
		);
		const field = screen.getByRole("textbox", { name: "Max run time" });
		await user.type(field, "90m");

		const message = await screen.findByText("Enter a time from 1h to 7d.", {
			selector: "p[id]",
		});
		await waitFor(() => expect(field).toHaveAttribute("aria-invalid", "true"));
		expect(field.getAttribute("aria-describedby")).toContain(message.id);
	}, 15_000);

	it("limits max run time in yaml", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `maxRuntime: ${body.policy?.spec.limits?.maxRuntime ?? "none"}\n`,
			}),
		);
		const user = await openNew();
		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await user.click(
			screen.getByRole("button", { name: "Limits · no limits" }),
		);
		await user.type(
			screen.getByRole("textbox", { name: "Max run time" }),
			"12h",
		);

		await waitFor(() => expect(yaml).toHaveValue("maxRuntime: 12h\n"));
		expect(requests.at(-1)?.policy?.spec.limits).toEqual({ maxRuntime: "12h" });
	}, 15_000);

	it("limits edit updates yaml", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `uploadRate: ${body.policy?.spec.limits?.uploadRate ?? "none"}\n`,
			}),
		);
		const user = await openNew();
		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await user.click(
			screen.getByRole("button", { name: "Limits · no limits" }),
		);
		const rate = screen.getByRole("textbox", { name: "Upload rate" });
		expect(rate).toHaveAttribute("placeholder", "Unlimited");
		await user.type(rate, "20MiB/s");

		await waitFor(() => expect(yaml).toHaveValue("uploadRate: 20MiB/s\n"));
		expect(requests.at(-1)?.policy?.spec.limits).toEqual({
			uploadRate: "20MiB/s",
		});
		expect(
			screen.getByRole("button", { name: "Limits · upload 20MiB/s" }),
		).toBeInTheDocument();
	}, 15_000);
});

describe("Managed policies", () => {
	const hint =
		"Managed by lode-apply. The next apply overwrites edits made here.";
	const alertText =
		"Managed from Git. This policy is applied by lode-apply. The next apply overwrites changes made here.";

	it("managed policy shows badge and alert", async () => {
		mockPolicies([
			summary("gitops", { managed_by: "lode-apply" }),
			summary("by-hand"),
		]);
		await openList();

		await screen.findByRole("link", { name: "Open gitops" });
		const rows = within(screen.getByRole("table")).getAllByRole("row");
		const badge = within(rows[1]).getByRole("img", { name: hint });
		expect(badge).toHaveTextContent("Managed");
		expect(within(rows[2]).queryByText("Managed")).not.toBeInTheDocument();
	});

	it("managed policy editor opens with a warning that does not close", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("gitops", { managed_by: "lode-apply" }));
		mockPolicy(makeDetail("by-hand"));
		renderApp("/policies/gitops");
		await screen.findByRole("heading", { name: "gitops" });

		expect(screen.getByText(alertText)).toBeVisible();
		expect(screen.getByRole("textbox", { name: "Name" })).toBeDisabled();
		expect(
			screen.queryByRole("button", { name: /dismiss|close/i }),
		).not.toBeInTheDocument();
	});

	it("unmanaged policy editor has no managed warning", async () => {
		useSignedIn();
		mockValidate();
		mockPolicy(makeDetail("by-hand"));
		renderApp("/policies/by-hand");
		await screen.findByRole("heading", { name: "by-hand" });

		expect(screen.queryByText(/Managed from Git/)).not.toBeInTheDocument();
	});

	it("managed save toast", async () => {
		useSignedIn();
		mockValidate();
		const served = makeDetail("gitops", { managed_by: "lode-apply" });
		mockPolicy(served);
		server.use(
			http.put("/api/v1/policies/gitops", () =>
				HttpResponse.json({ ...served, revision: 2 }),
			),
		);
		const user = userEvent.setup();
		renderApp("/policies/gitops");
		await screen.findByRole("heading", { name: "gitops" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);

		expect(
			await screen.findByText(
				'Policy "gitops" saved. The next apply from lode-apply overwrites it.',
			),
		).toBeInTheDocument();
	}, 15_000);

	it("delete dialog explains snapshots", async () => {
		mockPolicies([
			summary("gitops", { managed_by: "lode-apply" }),
			summary("by-hand"),
		]);
		const user = await openList();
		const snapshots =
			'Hosts stop being backed up by it. Snapshots it already created stay in the repository, are marked "Policy deleted" and are never forgotten automatically.';
		const git =
			"It is managed from Git and comes back on the next apply unless you remove it from the files.";

		await user.click(
			await screen.findByRole("button", { name: "Actions for by-hand" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		let dialog = await screen.findByRole("alertdialog");
		expect(dialog).toHaveTextContent(snapshots);
		expect(dialog).not.toHaveTextContent(git);
		await user.click(
			within(dialog).getByRole("button", { name: "Keep policy" }),
		);
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);

		await user.click(
			await screen.findByRole("button", { name: "Actions for gitops" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete policy" }),
		);
		dialog = await screen.findByRole("alertdialog");
		expect(dialog).toHaveTextContent(snapshots);
		expect(dialog).toHaveTextContent(git);
	}, 15_000);
});

describe("Host policies card", () => {
	it("host policies show next run", async () => {
		server.use(
			http.get("/api/v1/hosts/h1/policies", () =>
				HttpResponse.json([
					{
						name: "nightly",
						last_job: null,
						next_run: Date.now() + 2 * 3_600_000 + 60_000,
					},
					{ name: "by-hand", last_job: null },
				]),
			),
		);
		const root = createRootRoute({
			component: () => (
				<QueryClientProvider
					client={
						new QueryClient({ defaultOptions: { queries: { retry: false } } })
					}
				>
					<TooltipProvider>
						<HostPolicies host={{ id: "h1", status: "online" } as HostDetail} />
					</TooltipProvider>
				</QueryClientProvider>
			),
		});
		const router = createRouter({
			routeTree: root.addChildren([
				createRoute({ getParentRoute: () => root, path: "/policies/$name" }),
				createRoute({ getParentRoute: () => root, path: "/policies/new" }),
			]),
			history: createMemoryHistory({ initialEntries: ["/"] }),
		});
		render(<RouterProvider router={router} />);

		const nightly = (
			await screen.findByRole("link", { name: "nightly" })
		).closest("li") as HTMLElement;
		expect(within(nightly).getByText("in 2 hours")).toBeInTheDocument();
		expect(within(nightly).getByText(/^Next/)).toBeInTheDocument();
		const byHand = screen
			.getByRole("link", { name: "by-hand" })
			.closest("li") as HTMLElement;
		expect(within(byHand).queryByText(/^Next/)).not.toBeInTheDocument();
	});
});

function previewHost(
	name: string,
	total: number,
	forget: number,
): HostRetentionPreview {
	return {
		host_id: `${name}-id`,
		hostname: name,
		total,
		forget,
		snapshots: Array.from({ length: forget }, (_, i) => ({
			id: `${name}-snap-${i}`,
			time: Date.UTC(2026, 0, 10 + i, 9, 30),
			received_at: Date.UTC(2026, 0, 10 + i, 9, 31),
		})),
	};
}

function previewOf(over: Partial<RetentionPreview> = {}): RetentionPreview {
	return {
		total: 20,
		forget: 4,
		floor: 0,
		suspicious: 0,
		truncated: 0,
		hosts: [previewHost("web-1", 12, 3), previewHost("db-1", 8, 1)],
		...over,
	};
}

type PreviewBody = { retention: unknown; timezone?: string | null };

function mockPreview(
	name: string,
	handler: (
		body: PreviewBody,
		call: number,
	) => Response | RetentionPreview | Promise<Response | RetentionPreview>,
) {
	const bodies: PreviewBody[] = [];
	server.use(
		http.post(
			`/api/v1/policies/${name}/retention-preview`,
			async ({ request }) => {
				const body = (await request.json()) as PreviewBody;
				bodies.push(body);
				const out = await handler(body, bodies.length);
				return out instanceof Response ? out : HttpResponse.json(out);
			},
		),
	);
	return bodies;
}

async function openRetained(
	retention: BackupPolicy["spec"]["retention"] = { keepLast: 3 },
) {
	useSignedIn();
	mockValidate();
	mockPolicy(makeDetail("nightly", {}, { retention }));
	const user = userEvent.setup();
	renderApp("/policies/nightly");
	await screen.findByRole("heading", { name: "nightly" });
	return user;
}

describe("Retention section", () => {
	it("retention fields write yaml", async () => {
		const requests = mockValidate((body) =>
			validResponse({
				yaml: `keepLast: ${body.policy?.spec.retention?.keepLast ?? "none"}\n`,
			}),
		);
		const user = await openNew();
		expect(
			screen.getByText(
				"Keeps everything. Nothing is forgotten until you set a rule.",
			),
		).toBeVisible();
		expect(
			screen.getByText(
				"Applied per host to this policy's snapshots after each successful backup. Snapshots from lode runs and other policies are never touched.",
			),
		).toBeVisible();
		const keepWithin = screen.getByRole("textbox", { name: "Keep within" });
		expect(keepWithin).toHaveAttribute("placeholder", "30d");

		const last = screen.getByRole("textbox", { name: "Keep last" });
		expect(last).toHaveAttribute("inputmode", "numeric");
		await user.type(last, "7x");
		expect(last).toHaveValue("7");
		await user.type(keepWithin, "30d");

		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(yaml).toHaveValue("keepLast: 7\n"));
		expect(requests.at(-1)?.policy?.spec.retention).toEqual({
			keepLast: 7,
			keepWithin: "30d",
		});
		expect(
			screen.queryByText(
				"Keeps everything. Nothing is forgotten until you set a rule.",
			),
		).not.toBeInTheDocument();

		await user.clear(last);
		await user.clear(keepWithin);
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.retention).toBeUndefined(),
		);
	}, 15_000);

	it("retention yaml edit fills the fields", async () => {
		mockValidate((body) =>
			body.yaml
				? validResponse({
						policy: makePolicy("etc-and-home", {
							retention: { keepDaily: 14, keepWithin: "30d" },
						}),
					})
				: validResponse(),
		);
		const user = await openNew();
		const yaml = screen.getByRole("textbox", { name: "Policy YAML" });
		await waitFor(() => expect(yaml).not.toHaveValue(""));
		await user.clear(yaml);
		await user.click(yaml);
		await user.paste("kind: BackupPolicy\n");

		await waitFor(() =>
			expect(screen.getByRole("textbox", { name: "Keep daily" })).toHaveValue(
				"14",
			),
		);
		expect(screen.getByRole("textbox", { name: "Keep within" })).toHaveValue(
			"30d",
		);
	}, 15_000);

	it("retention field error shown", async () => {
		mockValidate((body) =>
			body.policy?.spec.retention?.keepLast === 0
				? {
						valid: false,
						errors: [
							{
								path: "spec.retention.keepLast",
								message: "Enter a whole number of 1 or more.",
							},
						],
						name_taken: false,
						warnings: [],
					}
				: validResponse(),
		);
		const user = await openNew();
		const last = screen.getByRole("textbox", { name: "Keep last" });
		await user.type(last, "0");

		const message = await screen.findByText(
			"Enter a whole number of 1 or more.",
			{ selector: "p[id]" },
		);
		await waitFor(() => expect(last).toHaveAttribute("aria-invalid", "true"));
		expect(last.getAttribute("aria-describedby")).toContain(message.id);
		expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
	}, 15_000);

	it("retention survives editing another field", async () => {
		const stored = { keepLast: 3, keepWithin: "30d", keepMonthly: 6 };
		const policy = makePolicy("nightly", { retention: stored });
		expect(policyFromDraft(draftFromPolicy(policy)).spec.retention).toEqual(
			stored,
		);

		const requests = mockValidate();
		useSignedIn();
		const served = makeDetail("nightly", {}, { retention: stored });
		mockPolicy(served);
		mockPreview("nightly", () => previewOf({ forget: 0, hosts: [] }));
		const saved: unknown[] = [];
		server.use(
			http.put("/api/v1/policies/nightly", async ({ request }) => {
				saved.push(await request.json());
				return HttpResponse.json({ ...served, revision: 2 });
			}),
		);
		const user = userEvent.setup();
		renderApp("/policies/nightly");
		await screen.findByRole("heading", { name: "nightly" });

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.retention).toEqual(stored),
		);
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);
		await waitFor(() => expect(saved).toHaveLength(1));
		expect(
			(saved[0] as { policy: BackupPolicy }).policy.spec.retention,
		).toEqual(stored);
	}, 20_000);
});

describe("Retention includeStdin", () => {
	it("includeStdin survives the form round trip", () => {
		const stored = { keepLast: 3, includeStdin: true };
		const policy = makePolicy("nightly", { retention: stored });
		expect(policyFromDraft(draftFromPolicy(policy)).spec.retention).toEqual(
			stored,
		);

		const plain = makePolicy("nightly", { retention: { keepLast: 3 } });
		expect(policyFromDraft(draftFromPolicy(plain)).spec.retention).toEqual({
			keepLast: 3,
		});
	});
});

describe("Retention preview", () => {
	it("retention preview summary and suspicious", async () => {
		const bodies = mockPreview("nightly", () =>
			previewOf({ suspicious: 2, truncated: 0, floor: 2 }),
		);
		const user = await openRetained();

		const summary = await screen.findByText(
			"Would forget 4 of 20 snapshots on 2 hosts.",
		);
		expect(summary).toHaveClass("font-semibold");
		expect(summary.parentElement?.parentElement).toHaveAttribute(
			"aria-live",
			"polite",
		);
		expect(
			screen.getByText(
				"2 suspicious snapshots are not counted and are never forgotten.",
			),
		).toBeVisible();
		expect(
			screen.getByText(
				"2 more snapshots are kept: the two newest complete snapshots of each host are never forgotten.",
			),
		).toBeVisible();
		expect(bodies[0]).toEqual({ retention: { keepLast: 3 }, timezone: "UTC" });
		expect(
			screen.queryByText(/Saving does not delete anything/),
		).not.toBeInTheDocument();

		await user.clear(screen.getByRole("textbox", { name: "Keep last" }));
		await user.type(screen.getByRole("textbox", { name: "Keep last" }), "5");
		await waitFor(() =>
			expect(bodies.at(-1)?.retention).toEqual({ keepLast: 5 }),
		);
		expect(
			await screen.findByText(/Saving does not delete anything by itself/),
		).toBeVisible();
	}, 20_000);

	it("retention preview says nothing is forgotten", async () => {
		mockPreview("nightly", () => previewOf({ forget: 0, hosts: [] }));
		await openRetained();

		expect(
			await screen.findByText("Nothing would be forgotten right now."),
		).toBeVisible();
		expect(
			screen.queryByRole("button", { name: "Show snapshots to forget" }),
		).not.toBeInTheDocument();
	});

	it("retention preview new policy hint", async () => {
		let calls = 0;
		server.use(
			http.post("/api/v1/policies/:name/retention-preview", () => {
				calls += 1;
				return HttpResponse.json(previewOf());
			}),
		);
		const requests = mockValidate();
		const user = await openNew();
		expect(
			screen.queryByText("The preview appears once the policy has snapshots."),
		).not.toBeInTheDocument();

		await user.type(screen.getByRole("textbox", { name: "Keep last" }), "5");
		expect(
			await screen.findByText(
				"The preview appears once the policy has snapshots.",
			),
		).toBeVisible();
		await waitFor(() =>
			expect(requests.at(-1)?.policy?.spec.retention?.keepLast).toBe(5),
		);
		await act(async () => {});
		expect(calls).toBe(0);
	}, 15_000);

	it("retention preview error retries", async () => {
		const bodies = mockPreview("nightly", (_, call) =>
			call === 1
				? HttpResponse.json(
						{ code: "internal", message: "boom" },
						{ status: 500 },
					)
				: previewOf(),
		);
		const user = await openRetained();

		const alert = await screen.findByText(
			"Couldn't calculate the preview. Check that the server is reachable, then retry.",
		);
		expect(alert).toBeVisible();
		await user.click(screen.getByRole("button", { name: "Retry loading" }));

		expect(
			await screen.findByText("Would forget 4 of 20 snapshots on 2 hosts."),
		).toBeVisible();
		expect(bodies).toHaveLength(2);
		expect(
			screen.queryByRole("button", { name: "Retry loading" }),
		).not.toBeInTheDocument();
	}, 20_000);
});

describe("Retention preview rejected", () => {
	it("retention preview 422 shows the error with retry", async () => {
		mockPreview("nightly", () =>
			HttpResponse.json(
				{ code: "invalid_argument", message: "bad timezone" },
				{ status: 422 },
			),
		);
		await openRetained();

		expect(
			await screen.findByText(
				"Couldn't calculate the preview. Check that the server is reachable, then retry.",
			),
		).toBeVisible();
		expect(
			screen.getByRole("button", { name: "Retry loading" }),
		).toBeInTheDocument();
	}, 20_000);
});

describe("Retention preview list and heavy warning", () => {
	const manyHosts = Array.from({ length: 12 }, (_, i) =>
		previewHost(`host-${String(i + 1).padStart(2, "0")}`, 9, 3),
	);

	it("retention preview groups by host", async () => {
		mockPreview("nightly", () =>
			previewOf({
				total: 108,
				forget: 36,
				truncated: 40,
				hosts: [...manyHosts, previewHost("quiet-1", 5, 0)],
			}),
		);
		const user = await openRetained();

		await screen.findByText("Would forget 36 of 108 snapshots on 12 hosts.");
		const trigger = screen.getByRole("button", {
			name: "Show snapshots to forget",
		});
		expect(trigger).toHaveAttribute("aria-expanded", "false");
		await user.click(trigger);
		expect(trigger).toHaveTextContent("Hide snapshots to forget");

		const links = () => screen.getAllByRole("link", { name: /^host-\d+$/ });
		expect(links()).toHaveLength(10);
		expect(links()[0]).toHaveAttribute("href", "/hosts/host-01-id");
		expect(screen.getAllByText("forget 3 of 9")).toHaveLength(10);
		expect(screen.getAllByText("Not kept by any rule")).toHaveLength(30);
		expect(screen.getAllByText("host-01-")).toHaveLength(3);
		expect(
			screen.getAllByText(formatDateTime(Date.UTC(2026, 0, 10, 9, 30)))[0],
		).toBeInTheDocument();
		expect(screen.queryByText("quiet-1")).not.toBeInTheDocument();
		expect(screen.getByText("and 40 more")).toBeVisible();

		await user.click(screen.getByRole("button", { name: "Show all 12 hosts" }));
		expect(links()).toHaveLength(12);
		expect(
			screen.queryByRole("button", { name: "Show all 12 hosts" }),
		).not.toBeInTheDocument();
	}, 20_000);

	it("retention preview heavy warning", async () => {
		mockPreview("nightly", (body) =>
			(body.retention as { keepLast?: number }).keepLast === 1
				? previewOf({ total: 20, forget: 12 })
				: previewOf({ total: 20, forget: 4 }),
		);
		const user = await openRetained({ keepLast: 3 });

		await screen.findByText("Would forget 4 of 20 snapshots on 2 hosts.");
		expect(screen.queryByText(/This forgets/)).not.toBeInTheDocument();

		const last = screen.getByRole("textbox", { name: "Keep last" });
		await user.clear(last);
		await user.type(last, "1");
		const warning = await screen.findByText(
			"This forgets 60% of the snapshots. Check the rules before you save.",
		);
		expect(warning.closest("[role=status]")).not.toBeNull();
		expect(
			screen.getByText("Would forget 12 of 20 snapshots on 2 hosts."),
		).toBeVisible();
	}, 20_000);
});

describe("Saving retention rules", () => {
	function mockSave(name: string) {
		const bodies: { policy: BackupPolicy }[] = [];
		server.use(
			http.put(`/api/v1/policies/${name}`, async ({ request }) => {
				bodies.push((await request.json()) as { policy: BackupPolicy });
				return HttpResponse.json({
					...makeDetail(name, {}, { retention: { keepLast: 2 } }),
					revision: 2,
				});
			}),
		);
		return bodies;
	}

	const forgetsFive = () =>
		previewOf({
			total: 20,
			forget: 5,
			hosts: [previewHost("web-1", 12, 3), previewHost("db-1", 8, 2)],
		});

	it("save retention confirmation", async () => {
		const bodies = mockSave("nightly");
		const previews = mockPreview("nightly", async () => {
			await delay(1200);
			return forgetsFive();
		});
		const user = await openRetained({ keepLast: 3 });
		await screen.findByText("Would forget 5 of 20 snapshots on 2 hosts.");

		const last = screen.getByRole("textbox", { name: "Keep last" });
		await user.clear(last);
		await user.type(last, "2");
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() =>
			expect(previews.at(-1)?.retention).toMatchObject({ keepLast: 2 }),
		);
		expect(save).toBeDisabled();
		await waitFor(() => expect(save).toBeEnabled(), { timeout: 4000 });
		expect(
			screen.getByText(/Saving does not delete anything by itself/),
		).toBeVisible();

		await user.click(save);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Save retention rules?",
		});
		expect(
			within(dialog).getByText(
				"After the next successful backup of each host, 5 snapshots on 2 hosts will be forgotten. This cannot be undone. Their data stays in storage until garbage collection.",
			),
		).toBeVisible();
		expect(within(dialog).getAllByText("web-1")).toHaveLength(3);
		expect(within(dialog).getAllByText("db-1")).toHaveLength(2);
		expect(within(dialog).getAllByText("web-1-sn")).toHaveLength(3);
		expect(within(dialog).queryByText(/more$/)).not.toBeInTheDocument();
		const keep = within(dialog).getByRole("button", { name: "Keep editing" });
		await waitFor(() => expect(keep).toHaveFocus());

		await user.click(keep);
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
		expect(bodies).toHaveLength(0);

		await user.click(screen.getByRole("button", { name: "Save policy" }));
		const again = await screen.findByRole("alertdialog");
		await user.click(
			within(again).getByRole("button", { name: "Save policy" }),
		);
		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(bodies[0].policy.spec.retention).toEqual({ keepLast: 2 });
	}, 30_000);

	it("save without confirmation when nothing forgotten", async () => {
		const bodies = mockSave("nightly");
		mockPreview("nightly", () => previewOf({ forget: 0, hosts: [] }));
		const user = await openRetained({ keepLast: 3 });
		await screen.findByText("Nothing would be forgotten right now.");

		const last = screen.getByRole("textbox", { name: "Keep last" });
		await user.clear(last);
		await user.type(last, "2");
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);

		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
	}, 20_000);

	it("save without confirmation when the rules did not change", async () => {
		const bodies = mockSave("nightly");
		mockPreview("nightly", forgetsFive);
		const user = await openRetained({ keepLast: 3 });
		await screen.findByText("Would forget 5 of 20 snapshots on 2 hosts.");

		await user.type(screen.getAllByRole("textbox", { name: "Paths" })[0], "/x");
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		expect(
			screen.queryByText(/Saving does not delete anything/),
		).not.toBeInTheDocument();
		await user.click(save);

		await waitFor(() => expect(bodies).toHaveLength(1));
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
		expect(bodies[0].policy.spec.retention).toEqual({ keepLast: 3 });
	}, 20_000);

	it("save without confirmation for the first rules of a new policy", async () => {
		useSignedIn();
		mockValidate();
		const created: unknown[] = [];
		const saved = makeDetail(
			"etc-and-home",
			{},
			{ retention: { keepLast: 4 } },
		);
		server.use(
			http.post("/api/v1/policies", async ({ request }) => {
				created.push(await request.json());
				return HttpResponse.json(saved, { status: 201 });
			}),
			http.get("/api/v1/policies/etc-and-home", () => HttpResponse.json(saved)),
		);
		const user = userEvent.setup();
		renderApp("/policies/new");
		await screen.findByRole("heading", { name: "New policy" });

		await user.type(screen.getByRole("textbox", { name: "Keep last" }), "4");
		const save = screen.getByRole("button", { name: "Save policy" });
		await waitFor(() => expect(save).toBeEnabled());
		await user.click(save);

		await waitFor(() => expect(created).toHaveLength(1));
		expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
		expect(
			(created[0] as { policy: BackupPolicy }).policy.spec.retention,
		).toEqual({ keepLast: 4 });
	}, 20_000);
});
