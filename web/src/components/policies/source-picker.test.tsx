import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { delay, HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	GuestOption,
	Source,
	SourceAvailability,
	SourceOptions,
	SourceOptionsRequest,
	ValidateRequest,
} from "@/api/gen/model";
import { copy } from "@/copy";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const available = (type: string): SourceAvailability => ({
	source_type: type,
	hosts_total: 1,
	hosts_with: 1,
	hosts_agent_outdated: 0,
});

function mockValidate(fromYaml?: Source) {
	const requests: ValidateRequest[] = [];
	server.use(
		http.post("/api/v1/policies/validate", async ({ request }) => {
			const body = (await request.json()) as ValidateRequest;
			requests.push(body);
			const policy =
				body.yaml != null && fromYaml
					? {
							apiVersion: "lodestore.io/v1alpha2",
							kind: "BackupPolicy",
							metadata: { name: "from-yaml" },
							spec: {
								repository: "main",
								selector: { matchLabels: { role: "pve" } },
								source: fromYaml,
								paths: [],
								exclude: [],
							},
						}
					: body.policy;
			return HttpResponse.json({
				valid: true,
				errors: [],
				name_taken: false,
				warnings: [],
				yaml: `source: ${JSON.stringify(policy?.spec.source ?? null)}\n`,
				policy,
				matching: { hosts: [], other_repository: [] },
				availability: [
					available("proxmox-vm"),
					available("proxmox-ct"),
					available("postgres"),
					available("mysql"),
				],
			});
		}),
	);
	return requests;
}

const empty: SourceOptions = {
	guests: [],
	pools: [],
	tags: [],
	databases: [],
	truncated_hosts: [],
	unreachable_hosts: [],
};

function mockOptions(
	answer:
		| SourceOptions
		| ((request: SourceOptionsRequest) => Response | SourceOptions),
) {
	const requests: SourceOptionsRequest[] = [];
	server.use(
		http.post("/api/v1/policies/source-options", async ({ request }) => {
			const body = (await request.json()) as SourceOptionsRequest;
			requests.push(body);
			const out = typeof answer === "function" ? answer(body) : answer;
			return out instanceof Response ? out : HttpResponse.json(out);
		}),
	);
	return requests;
}

const guest = (
	vmid: number,
	name: string,
	over: Partial<GuestOption> = {},
): GuestOption => ({
	vmid,
	kind: "vm",
	name,
	pool: null,
	tags: [],
	hosts: ["pve-01"],
	...over,
});

const manyGuests = (n: number) =>
	Array.from({ length: n }, (_, i) => guest(i + 1, `vm-${i + 1}`));

type User = ReturnType<typeof userEvent.setup>;

async function typeLabel(user: User) {
	await user.type(screen.getByLabelText(copy.labels.key), "role");
	await user.type(screen.getByLabelText(copy.labels.value), "pve");
}

async function openForm(type: RegExp, withHosts = true) {
	useSignedIn();
	const user = userEvent.setup();
	renderApp("/policies/new");
	await screen.findByRole("heading", { name: "New policy" });
	if (withHosts) await typeLabel(user);
	await user.click(await screen.findByRole("radio", { name: type }));
	return user;
}

async function loadYaml(user: User) {
	await user.click(screen.getByRole("tab", { name: "YAML" }));
	const field = await screen.findByRole("textbox", { name: "Policy YAML" });
	await user.clear(field);
	await user.click(field);
	await user.paste("kind: BackupPolicy\n");
	await new Promise((resolve) => setTimeout(resolve, 700));
	await user.click(screen.getByRole("tab", { name: "Form" }));
}

const lastSource = (requests: ValidateRequest[]) =>
	requests.at(-1)?.policy?.spec.source;

describe("Source picker", () => {
	it("turns two ticked guests into vmids in the YAML and the saved policy", async () => {
		const validated = mockValidate();
		const requests = mockOptions({
			...empty,
			guests: [
				guest(210, "mail", { hosts: ["pve-02"] }),
				guest(101, "web-01"),
				guest(103, "db-01"),
			],
		});
		const created: unknown[] = [];
		server.use(
			http.post("/api/v1/policies", async ({ request }) => {
				created.push(await request.json());
				return HttpResponse.json({}, { status: 500 });
			}),
		);
		const user = await openForm(/Virtual machines/);

		expect(
			await screen.findByRole("checkbox", { name: "VM 101 · web-01" }),
		).toBeVisible();
		expect(
			screen.getByRole("searchbox", { name: "Search 3 virtual machines" }),
		).toBeVisible();
		expect(requests.at(-1)).toMatchObject({
			repository: "main",
			selector: { matchLabels: { role: "pve" } },
			source_type: "proxmox-vm",
		});
		const save = screen.getByRole("button", { name: "Save policy" });
		expect(save).toBeDisabled();
		expect(screen.getByText("Pick at least one guest.")).toBeVisible();
		expect(lastSource(validated)).toBeUndefined();

		await user.click(screen.getByRole("checkbox", { name: "VM 103 · db-01" }));
		await user.click(screen.getByRole("checkbox", { name: "VM 101 · web-01" }));

		expect(screen.getByText("2 guests selected")).toHaveAttribute(
			"role",
			"status",
		);
		expect(
			screen.queryByText("Pick at least one guest."),
		).not.toBeInTheDocument();
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "proxmox-vm",
				vmids: [101, 103],
			}),
		);
		await waitFor(() => expect(save).toBeEnabled());

		await user.click(screen.getByRole("tab", { name: "YAML" }));
		await waitFor(() =>
			expect(screen.getByRole("textbox", { name: "Policy YAML" })).toHaveValue(
				'source: {"type":"proxmox-vm","vmids":[101,103]}\n',
			),
		);
		await user.click(screen.getByRole("tab", { name: "Form" }));
		await user.click(screen.getByRole("button", { name: "Save policy" }));
		await waitFor(() => expect(created).toHaveLength(1));
		expect(created[0]).toMatchObject({
			policy: {
				spec: { source: { type: "proxmox-vm", vmids: [101, 103] } },
			},
		});
	});

	it("picks a pool or tags", async () => {
		const validated = mockValidate();
		mockOptions({
			...empty,
			guests: [guest(300, "db-01", { tags: ["db"] })],
			pools: [{ name: "prod", count: 12 }],
			tags: [
				{ name: "db", count: 1 },
				{ name: "web", count: 2 },
			],
		});
		const user = await openForm(/Virtual machines/);

		await user.click(await screen.findByRole("radio", { name: "By pool" }));
		await user.click(screen.getByRole("combobox", { name: "By pool" }));
		expect(
			await screen.findByRole("option", { name: /12 guests/ }),
		).toBeVisible();
		await user.click(screen.getByRole("option", { name: /prod/ }));
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "proxmox-vm",
				pool: "prod",
			}),
		);

		await user.click(screen.getByRole("radio", { name: "By tags" }));
		await user.click(await screen.findByRole("button", { name: "web 2" }));
		await user.click(screen.getByRole("button", { name: "db 1" }));
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "proxmox-vm",
				tags: ["db", "web"],
			}),
		);
	});

	it("explains an empty selection, missing guests, pools and tags", async () => {
		mockValidate();
		mockOptions(empty);
		const user = await openForm(/Virtual machines/, false);

		expect(
			await screen.findByText(
				"Select hosts first to see what the agent found on them.",
			),
		).toBeVisible();
		expect(screen.queryByRole("searchbox")).not.toBeInTheDocument();

		await typeLabel(user);
		expect(
			await screen.findByText("No guests found on the selected hosts."),
		).toBeVisible();
		await user.click(screen.getByRole("radio", { name: "By pool" }));
		expect(screen.getByRole("combobox", { name: "By pool" })).toBeDisabled();
		expect(
			screen.getByText("No pools found on the selected hosts."),
		).toBeVisible();
		await user.click(screen.getByRole("radio", { name: "By tags" }));
		expect(
			screen.getByText("No tags found on the selected hosts."),
		).toBeVisible();
	});

	it("stops at 20 tags", async () => {
		const validated = mockValidate();
		const name = (i: number) => `t${String(i).padStart(2, "0")}`;
		mockOptions({
			...empty,
			tags: Array.from({ length: 21 }, (_, i) => ({ name: name(i), count: 1 })),
		});
		const user = await openForm(/Virtual machines/);
		await user.click(await screen.findByRole("radio", { name: "By tags" }));
		for (let i = 0; i < 20; i++) {
			await user.click(screen.getByRole("button", { name: `${name(i)} 1` }));
		}
		expect(screen.getByRole("button", { name: "t20 1" })).toBeDisabled();
		expect(screen.getByText("At most 20 tags.")).toBeVisible();
		await waitFor(() =>
			expect((lastSource(validated) as { tags: string[] }).tags).toHaveLength(
				20,
			),
		);
	});

	it("picks databases, schemas and what to leave out", async () => {
		const validated = mockValidate();
		mockOptions((request) => ({
			...empty,
			databases: ["app", "billing", "scratch"].map((name) => ({
				name,
				source_type: request.source_type,
				hosts: ["db-02"],
			})),
		}));
		const user = await openForm(/PostgreSQL/);

		expect(
			await screen.findByText(
				"Every database on each selected host, including ones created later.",
			),
		).toBeVisible();
		expect(screen.queryByRole("table")).not.toBeInTheDocument();
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({ type: "postgres" }),
		);

		await user.click(screen.getByRole("radio", { name: "Pick databases" }));
		expect(
			await screen.findByRole("columnheader", { name: "Database" }),
		).toBeVisible();
		expect(screen.getByText("Pick at least one database.")).toBeVisible();
		await user.click(screen.getByRole("checkbox", { name: "billing" }));
		await user.click(screen.getByRole("checkbox", { name: "app" }));
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "postgres",
				databases: ["app", "billing"],
			}),
		);

		await user.click(screen.getByRole("radio", { name: "All except" }));
		expect(screen.getByText("0 databases excluded")).toBeVisible();
		await user.click(await screen.findByRole("checkbox", { name: "scratch" }));
		expect(screen.getByText("1 database excluded")).toBeVisible();
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "postgres",
				excludeDatabases: ["scratch"],
			}),
		);

		await user.click(screen.getByRole("radio", { name: /MySQL/ }));
		expect(screen.getByRole("radio", { name: "All schemas" })).toBeVisible();
		await user.click(screen.getByRole("radio", { name: "All schemas" }));
		expect(
			screen.getByText(
				"Every schema on each selected host, including ones created later.",
			),
		).toBeVisible();
	});

	it("keeps ticked values the agents no longer report", async () => {
		const validated = mockValidate({ type: "proxmox-vm", vmids: [101, 150] });
		mockOptions({
			...empty,
			guests: [guest(101, "web-01"), guest(103, "db-01")],
		});
		const user = await openForm(/Virtual machines/);
		await loadYaml(user);

		const missing = await screen.findByRole("checkbox", { name: "VM 150" });
		expect(missing).toBeChecked();
		expect(
			within(screen.getAllByRole("row")[1]).getAllByText(
				"Not found on the selected hosts",
			),
		).not.toHaveLength(0);
		expect(
			screen.getByRole("checkbox", { name: "VM 101 · web-01" }),
		).toBeChecked();

		await user.click(missing);
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "proxmox-vm",
				vmids: [101],
			}),
		);
		expect(
			screen.queryByRole("checkbox", { name: "VM 150" }),
		).not.toBeInTheDocument();
	});

	it("leaves a selection that combines modes to the YAML tab", async () => {
		mockValidate({ type: "proxmox-vm", vmids: [101], pool: "prod" });
		mockOptions({ ...empty, guests: [guest(101, "web-01")] });
		const user = await openForm(/Virtual machines/);
		await loadYaml(user);

		expect(
			await screen.findByText(
				"This selection combines VMIDs, pool or tags. Edit it in the YAML tab.",
			),
		).toBeVisible();
		expect(screen.getByRole("radio", { name: "By pool" })).toBeDisabled();
		expect(screen.queryByRole("table")).not.toBeInTheDocument();
		expect(
			screen.queryByText("Pick at least one guest."),
		).not.toBeInTheDocument();
	});

	it("filters, selects everything under the filter and pages", async () => {
		const validated = mockValidate();
		mockOptions({ ...empty, guests: manyGuests(38) });
		const user = await openForm(/Virtual machines/);

		await screen.findByRole("checkbox", { name: "VM 1 · vm-1" });
		expect(screen.getAllByRole("row")).toHaveLength(26);
		expect(screen.getByText("Showing 25 of 38 guests")).toBeVisible();
		await user.click(screen.getByRole("button", { name: "Show 25 more" }));
		expect(screen.getAllByRole("row")).toHaveLength(39);
		expect(
			screen.queryByRole("button", { name: "Show 25 more" }),
		).not.toBeInTheDocument();
		expect(
			screen.getByRole("checkbox", { name: "VM 26 · vm-26" }),
		).toHaveFocus();

		const search = screen.getByRole("searchbox", {
			name: "Search 38 virtual machines",
		});
		await user.type(search, "ci");
		expect(screen.getByText('No guests match "ci".')).toBeVisible();
		expect(screen.queryByRole("table")).not.toBeInTheDocument();

		await user.clear(search);
		await user.click(screen.getByRole("checkbox", { name: "VM 5 · vm-5" }));
		expect(
			screen.getByRole("checkbox", { name: "Select all guests" }),
		).toHaveAttribute("aria-checked", "mixed");

		await user.type(search, "vm-3");
		expect(screen.getAllByRole("row")).toHaveLength(11);
		await user.click(
			screen.getByRole("checkbox", { name: "Select all guests" }),
		);
		expect(screen.getByText("11 guests selected")).toBeVisible();
		await user.click(
			screen.getByRole("checkbox", { name: "Select all guests" }),
		);
		expect(screen.getByText("1 guest selected")).toBeVisible();
		await waitFor(() =>
			expect(lastSource(validated)).toEqual({
				type: "proxmox-vm",
				vmids: [5],
			}),
		);
	});

	it("notes what the agents could not list", async () => {
		mockValidate();
		mockOptions((request) => ({
			...empty,
			guests: [guest(101, "web-01")],
			databases: [
				{ name: "app", source_type: request.source_type, hosts: ["db-02"] },
			],
			truncated_hosts: [{ hostname: "pve-01", count: 200 }],
			unreachable_hosts: request.source_type === "postgres" ? ["db-02"] : [],
		}));
		const user = await openForm(/Virtual machines/);
		expect(
			await screen.findByText(
				"The agent on pve-01 reported the first 200 guests only. Use a pool or tags to include the rest.",
			),
		).toBeVisible();

		await user.click(screen.getByRole("radio", { name: /PostgreSQL/ }));
		expect(
			await screen.findByText(
				"The agent on pve-01 reported the first 200 databases only. Use All except to include the rest.",
			),
		).toBeVisible();
		expect(
			screen.getByText(
				"The agent can't list the databases on db-02, so they are not in this list.",
			),
		).toBeVisible();
	});

	it("offers a retry when the lookup fails", async () => {
		mockValidate();
		let fail = true;
		mockOptions(() =>
			fail
				? HttpResponse.json(
						{ code: "internal", message: "boom" },
						{ status: 500 },
					)
				: { ...empty, guests: [guest(101, "web-01")] },
		);
		const user = await openForm(/Virtual machines/);

		expect(
			await screen.findByText(
				"Couldn't load the discovered sources. Check that the server is reachable, then retry.",
			),
		).toBeVisible();
		fail = false;
		await user.click(
			screen.getByRole("button", { name: "Retry loading discovered sources" }),
		);
		expect(
			await screen.findByRole("checkbox", { name: "VM 101 · web-01" }),
		).toBeVisible();
	});

	it("disables the search while the lookup is running", async () => {
		mockValidate();
		server.use(
			http.post("/api/v1/policies/source-options", async () => {
				await delay(500);
				return HttpResponse.json({ ...empty, guests: [guest(101, "web-01")] });
			}),
		);
		await openForm(/Virtual machines/);

		expect(
			await screen.findByRole("searchbox", { hidden: true }),
		).toBeDisabled();
		expect(
			await screen.findByRole("checkbox", { name: "VM 101 · web-01" }),
		).toBeVisible();
	});
});
