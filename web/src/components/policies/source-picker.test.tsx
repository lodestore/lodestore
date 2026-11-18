import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type {
	GuestOption,
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

function mockValidate() {
	const requests: ValidateRequest[] = [];
	server.use(
		http.post("/api/v1/policies/validate", async ({ request }) => {
			const body = (await request.json()) as ValidateRequest;
			requests.push(body);
			return HttpResponse.json({
				valid: true,
				errors: [],
				name_taken: false,
				warnings: [],
				yaml: `source: ${JSON.stringify(body.policy?.spec.source ?? null)}\n`,
				policy: body.policy,
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
	answer: SourceOptions | ((request: SourceOptionsRequest) => Response),
) {
	const requests: SourceOptionsRequest[] = [];
	server.use(
		http.post("/api/v1/policies/source-options", async ({ request }) => {
			const body = (await request.json()) as SourceOptionsRequest;
			requests.push(body);
			return typeof answer === "function"
				? answer(body)
				: HttpResponse.json(answer);
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

async function openForm(type: RegExp) {
	useSignedIn();
	const user = userEvent.setup();
	renderApp("/policies/new");
	await screen.findByRole("heading", { name: "New policy" });
	await user.type(screen.getByLabelText(copy.labels.key), "role");
	await user.type(screen.getByLabelText(copy.labels.value), "pve");
	await user.click(await screen.findByRole("radio", { name: type }));
	return user;
}

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
		expect(validated.at(-1)?.policy?.spec.source).toBeUndefined();

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
			expect(validated.at(-1)?.policy?.spec.source).toEqual({
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
});
