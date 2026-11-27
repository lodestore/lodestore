import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { formatBytes } from "@/lib/format";
import { encodeB64 } from "@/lib/names";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import {
	CT_HOST,
	CT_SNAP,
	containerFacts,
	ctSnapshot,
	dropped,
	guestTargets,
	mockContainer,
	NODE_2,
	replaceable,
} from "@/test/container";
import { server } from "@/test/server";

const PAGE = `/restore?host=${CT_HOST}&snapshot=${CT_SNAP}`;

async function open(options: Parameters<typeof mockContainer>[0] = {}) {
	const mock = mockContainer(options);
	useSignedIn();
	const app = renderApp(PAGE);
	const user = userEvent.setup();
	const button = await screen.findByRole("button", {
		name: "Restore container",
	});
	await user.click(button);
	const dialog = await screen.findByRole("dialog");
	return { mock, app, user, dialog, button };
}

async function pick(
	user: ReturnType<typeof userEvent.setup>,
	dialog: HTMLElement,
	combobox: string,
	option: string | RegExp,
) {
	await user.click(within(dialog).getByRole("combobox", { name: combobox }));
	await user.click(await screen.findByRole("option", { name: option }));
}

const replaceRadio = (dialog: HTMLElement) =>
	within(dialog).getByRole("radio", { name: "Replace an existing container" });

describe("Container restore dialog", { timeout: 30_000 }, () => {
	it("the header of a container snapshot shows its facts and the restore button; a file snapshot has none", async () => {
		mockContainer();
		useSignedIn();
		renderApp(PAGE);
		const button = await screen.findByRole("button", {
			name: "Restore container",
		});
		expect(button).toBeVisible();
		expect(await screen.findByText("CT 200 · dns")).toBeVisible();
		expect(screen.getByText("Snapshot", { selector: "dd" })).toBeVisible();
		expect(screen.getByText("rootfs 8 GiB · mp0 32 GiB")).toBeVisible();
		expect(screen.getByText("Unprivileged, 0 → 100000")).toBeVisible();
		expect(screen.getByText("21.4 GiB")).toBeVisible();
	});

	it("the browser labels the folder that is a mount point with its volume", async () => {
		mockContainer({ entries: { path: "/mnt", names: ["data", "other"] } });
		useSignedIn();
		renderApp(`${PAGE}&path=${encodeB64(new TextEncoder().encode("/mnt"))}`);
		const row = (
			await screen.findByRole("button", { name: "Open folder data" })
		).parentElement;
		expect(row).toHaveTextContent("mp0");
		const other = screen.getByRole("button", {
			name: "Open folder other",
		}).parentElement;
		expect(other).not.toHaveTextContent("mp0");
	});

	it("no restore button on a file snapshot", async () => {
		mockContainer({
			snapshot: { ...ctSnapshot, source_type: "files", source_label: null },
		});
		useSignedIn();
		renderApp(PAGE);
		await screen.findByText("Snapshot", { selector: "h2" });
		expect(
			screen.queryByRole("button", { name: "Restore container" }),
		).toBeNull();
	});

	it("new container: shows what happens and what is dropped before the start, then queues the job", async () => {
		const { mock, app, user, dialog } = await open();
		const view = within(dialog);

		expect(
			view.getByRole("heading", { name: "Restore CT 200 · dns" }),
		).toBeVisible();
		expect(
			await view.findByRole("combobox", { name: "Node" }),
		).toHaveTextContent("pve-01");
		expect(view.getByRole("combobox", { name: "Storage" })).toHaveTextContent(
			"local-lvm",
		);
		expect(view.getByRole("textbox", { name: "VMID" })).toHaveValue("105");
		expect(
			view.getByText("105 is the next free ID in pve-prod. Name: dns."),
		).toBeVisible();
		expect(
			view.getByText("Stays stopped and won't start on boot"),
		).toBeVisible();
		expect(view.getByText("Gets new MAC addresses")).toBeVisible();
		expect(await view.findByText("Runs unprivileged")).toBeVisible();
		expect(view.getByText("1 config key won't be restored")).toBeVisible();
		expect(view.getByText("lxc.apparmor.profile: unconfined")).toBeVisible();
		expect(
			view.getByText("Raw LXC settings can give root on the node"),
		).toBeVisible();
		expect(view.getByText("Network interfaces kept")).toBeVisible();
		expect(view.getByText("net0: bridge=vmbr0,name=eth0,tag=40")).toBeVisible();
		expect(
			view.getByText(/applies the same filter and lists what it dropped/),
		).toBeVisible();

		await user.click(view.getByRole("button", { name: "Restore as CT 105" }));
		await waitFor(() =>
			expect(app.router.state.location.pathname).toBe("/jobs/job-1"),
		);
		expect(mock.posts).toEqual([
			{
				snapshot_id: CT_SNAP,
				target_host_id: "h-pve-01",
				storage: "local-lvm",
				vmid: 105,
				accept_unverified: false,
			},
		]);
	});

	it("asks the preview again for the node that is picked", async () => {
		const { mock, user, dialog } = await open({
			factsFor: (hostId) =>
				hostId === NODE_2
					? containerFacts({
							dropped: [
								...dropped(1),
								{
									key: "net0",
									value: "bridge=vmbr0,name=eth0,tag=40",
									reason: "network",
								},
							],
							network: [],
						})
					: containerFacts(),
		});
		const view = within(dialog);
		await view.findByText("net0: bridge=vmbr0,name=eth0,tag=40");
		expect(mock.previews).toContain("h-pve-01");

		await pick(user, dialog, "Node", "pve-02");
		await waitFor(() => expect(mock.previews).toContain(NODE_2));
		expect(
			await view.findByText("2 config keys won't be restored"),
		).toBeVisible();
		expect(
			view.getByText(/uses a bridge that this node doesn't have/),
		).toBeVisible();
		expect(view.queryByText("Network interfaces kept")).toBeNull();
	});

	it("loads: nothing can start; a failed load offers Retry; a failed preview does not block", async () => {
		mockContainer({ targetsStatus: 500 });
		useSignedIn();
		renderApp(PAGE);
		const user = userEvent.setup();
		await user.click(
			await screen.findByRole("button", { name: "Restore container" }),
		);
		const dialog = await screen.findByRole("dialog");
		expect(
			await within(dialog).findByText(
				"Couldn't load the nodes of this cluster.",
			),
		).toBeVisible();
		let reloaded = false;
		server.use(
			http.get("/api/v1/restore/guest-targets", () => {
				reloaded = true;
				return HttpResponse.json(guestTargets());
			}),
		);
		await user.click(within(dialog).getByRole("button", { name: "Retry" }));
		await waitFor(() => expect(reloaded).toBe(true));
		expect(
			await within(dialog).findByRole("button", { name: "Restore as CT 105" }),
		).toBeEnabled();
	});

	it("a preview that fails says the agent still filters and leaves the start enabled; plugin and offline nodes are labelled", async () => {
		const { user, dialog } = await open({ previewStatus: 422 });
		const view = within(dialog);
		expect(
			await view.findByText(
				"Couldn't preview the config. The agent still filters it and lists what it dropped in the job.",
			),
		).toBeVisible();
		expect(
			view.getByRole("button", { name: "Restore as CT 105" }),
		).toBeEnabled();

		await user.click(view.getByRole("combobox", { name: "Node" }));
		expect(
			await screen.findByRole("option", {
				name: "pve-03 · Plugin not installed",
			}),
		).toHaveAttribute("aria-disabled", "true");
		expect(
			screen.getByRole("option", { name: "pve-04 · Offline" }),
		).not.toHaveAttribute("aria-disabled", "true");
	});

	it("a taken VMID is a field error and blocks the start", async () => {
		const { user, dialog } = await open({
			targets: guestTargets(replaceable()),
		});
		const view = within(dialog);
		const field = await view.findByRole("textbox", { name: "VMID" });
		await user.clear(field);
		await user.type(field, "200");
		expect(
			view.getByText(
				"VMID 200 is taken. Pick another or leave the suggested one.",
			),
		).toBeVisible();
		expect(
			view.getByRole("button", { name: "Restore as CT 200" }),
		).toBeDisabled();
	});

	it("nothing dropped says so; more than six are folded", async () => {
		const { user, dialog } = await open({
			facts: containerFacts({ dropped: [] }),
		});
		expect(
			await within(dialog).findByText("All config keys are restored."),
		).toBeVisible();
		expect(within(dialog).queryByText(/won't be restored/)).toBeNull();
		await user.click(screen.getByRole("button", { name: "Cancel" }));
	});

	it("nine dropped keys show six and Show 3 more", async () => {
		const { user, dialog } = await open({
			facts: containerFacts({ dropped: dropped(9) }),
		});
		const view = within(dialog);
		expect(
			await view.findByText("9 config keys won't be restored"),
		).toBeVisible();
		expect(view.getByText("lxc.key5: value-5")).toBeVisible();
		expect(view.queryByText("lxc.key6: value-6")).toBeNull();
		await user.click(view.getByRole("button", { name: "Show 3 more" }));
		expect(view.getByText("lxc.key8: value-8")).toBeVisible();
	});

	it("replace is off when no node allows it, and says how to turn it on", async () => {
		const { dialog } = await open();
		const radio = replaceRadio(dialog);
		expect(radio).toBeDisabled();
		expect(radio).toHaveAccessibleDescription(
			"Replacing is turned off on pve-01. To allow it, run lodestore-agent pve install --allow-guest-in-place on the node.",
		);
	});

	it("replace: running and HA containers are disabled, the warning lists volumes with no copy, the confirmation resets, step-up gates the request", async () => {
		const { mock, app, user, dialog } = await open({
			targets: guestTargets(replaceable()),
			stepUp: true,
		});
		const view = within(dialog);
		await user.click(await waitFor(() => replaceRadio(dialog)));

		const select = view.getByRole("combobox", { name: "Container" });
		await user.click(select);
		expect(
			await screen.findByRole("option", {
				name: "CT 201 · mail on pve-01 · running",
			}),
		).toHaveAttribute("aria-disabled", "true");
		expect(
			screen.getByRole("option", {
				name: "CT 202 · ha-web on pve-01 · managed by HA",
			}),
		).toHaveAttribute("aria-disabled", "true");
		await user.click(
			screen.getByRole("option", { name: "CT 200 · dns on pve-01 · stopped" }),
		);

		expect(
			view.getByText(
				"All volumes of CT 200 · dns are replaced with the snapshot",
			),
		).toBeVisible();
		expect(
			view.getByText("Deleted with no copy in this snapshot:"),
		).toBeVisible();
		const rows = view.getAllByRole("listitem").map((li) => li.textContent);
		expect(rows).toContain(
			`mp1 /cache · ${formatBytes(2 * 1024 ** 4)} — excluded from backups, not in this snapshot`,
		);
		expect(rows).toContain(
			"unused0 · 4 GiB — excluded from backups, not in this snapshot",
		);

		const replace = view.getByRole("button", { name: "Replace CT 200" });
		expect(replace).toBeDisabled();
		const confirm = view.getByRole("checkbox", {
			name: "Replace CT 200 · dns on pve-01",
		});
		await user.click(confirm);
		expect(replace).toBeEnabled();

		await user.click(replace);
		const stepUp = await screen.findByRole("dialog", {
			name: "Confirm your password",
		});
		await user.type(
			within(stepUp).getByLabelText("Password"),
			"hunter2hunter2",
		);
		await user.click(within(stepUp).getByRole("button", { name: "Confirm" }));
		await waitFor(() =>
			expect(app.router.state.location.pathname).toBe("/jobs/job-1"),
		);
		expect(mock.posts).toEqual([
			{
				snapshot_id: CT_SNAP,
				target_host_id: "h-pve-01",
				storage: "local-lvm",
				replace_vmid: 200,
				confirm_other_guest: false,
			},
		]);
	});

	it("a target without volumes lost has no deletion line; a cancelled step-up sends nothing", async () => {
		const targets = guestTargets(replaceable());
		targets.replace = [
			{
				...targets.replace[0],
				volumes: targets.replace[0].volumes.filter(
					(v) => !v.destroyed_without_copy,
				),
			},
		];
		const { mock, app, user, dialog } = await open({ targets, stepUp: true });
		const view = within(dialog);
		await user.click(await waitFor(() => replaceRadio(dialog)));
		expect(
			await view.findByText(/are replaced with the snapshot/),
		).toBeVisible();
		expect(
			view.queryByText("Deleted with no copy in this snapshot:"),
		).toBeNull();
		await user.click(
			view.getByRole("checkbox", { name: "Replace CT 200 · dns on pve-01" }),
		);
		await user.click(view.getByRole("button", { name: "Replace CT 200" }));
		const stepUp = await screen.findByRole("dialog", {
			name: "Confirm your password",
		});
		await user.click(within(stepUp).getByRole("button", { name: "Cancel" }));
		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(mock.posts).toHaveLength(0);
		expect(app.router.state.location.pathname).toBe("/restore");
		expect(within(dialog).queryByRole("alert")).toBeNull();
	});

	it("another guest is warned about and confirmed in the request; a snapshot without uid gets its own warning", async () => {
		const targets = guestTargets(replaceable());
		targets.replace = [{ ...targets.replace[0], other_guest: true }];
		const { mock, user, dialog } = await open({ targets });
		const view = within(dialog);
		await user.click(await waitFor(() => replaceRadio(dialog)));
		await pick(user, dialog, "Container", /CT 200/);
		expect(
			view.getByText(/This snapshot is of another container than CT 200/),
		).toBeVisible();
		await user.click(
			view.getByRole("checkbox", { name: "Replace CT 200 · dns on pve-01" }),
		);
		await user.click(view.getByRole("button", { name: "Replace CT 200" }));
		await waitFor(() => expect(mock.posts).toHaveLength(1));
		expect(mock.posts[0]).toMatchObject({
			replace_vmid: 200,
			confirm_other_guest: true,
		});
	});

	it("a snapshot without uid warns that it does not say where it came from", async () => {
		const targets = guestTargets(replaceable());
		targets.snapshot_guest = { vmid: 200, name: "dns" };
		targets.replace = [{ ...targets.replace[0], other_guest: true }];
		const { user, dialog } = await open({ targets });
		await user.click(await waitFor(() => replaceRadio(dialog)));
		await pick(user, dialog, "Container", /CT 200/);
		expect(
			within(dialog).getByText(
				"This snapshot doesn't say which container it came from. Check that it is the right one before you replace.",
			),
		).toBeVisible();
	});

	it("stale node facts: an alert with Retry that reloads the targets and clears the confirmation", async () => {
		const { user, dialog } = await open({
			targets: guestTargets(replaceable()),
			post: () => apiError(409, "guest_facts_stale", "stale"),
		});
		const view = within(dialog);
		await user.click(await waitFor(() => replaceRadio(dialog)));
		await pick(user, dialog, "Container", /CT 200/);
		const confirm = view.getByRole("checkbox", {
			name: "Replace CT 200 · dns on pve-01",
		});
		await user.click(confirm);
		await user.click(view.getByRole("button", { name: "Replace CT 200" }));
		expect(
			await view.findByText(
				"The node hasn't reported its containers recently. Retry in a minute.",
			),
		).toBeVisible();
		await user.click(view.getByRole("button", { name: "Retry" }));
		await waitFor(() =>
			expect(
				view.queryByText(/hasn't reported its containers recently/),
			).toBeNull(),
		);
		await waitFor(() => expect(confirm).not.toBeChecked());
	});

	it("a target with too many volumes to verify, and a running container, are refused with their own text", async () => {
		let code = "guest_target_unverifiable";
		const { user, dialog } = await open({
			targets: guestTargets(replaceable()),
			post: () => apiError(422, code, "CT 200 is running."),
		});
		const view = within(dialog);
		await user.click(await waitFor(() => replaceRadio(dialog)));
		await pick(user, dialog, "Container", /CT 200/);
		await user.click(
			view.getByRole("checkbox", { name: "Replace CT 200 · dns on pve-01" }),
		);
		await user.click(view.getByRole("button", { name: "Replace CT 200" }));
		expect(
			await view.findByText(
				"CT 200 has too many volumes to check. Replace it in Proxmox instead.",
			),
		).toBeVisible();
		code = "guest_running";
		await user.click(view.getByRole("button", { name: "Retry" }));
		await user.click(
			await view.findByRole("checkbox", {
				name: "Replace CT 200 · dns on pve-01",
			}),
		);
		await user.click(view.getByRole("button", { name: "Replace CT 200" }));
		expect(
			await view.findByText("Couldn't start the restore. CT 200 is running."),
		).toBeVisible();
	});

	it("the server saying the VMID is taken shows it on the field", async () => {
		const { user, dialog } = await open({
			post: () => apiError(409, "restore_vmid_taken", "taken"),
		});
		const view = within(dialog);
		await user.click(
			await view.findByRole("button", { name: "Restore as CT 105" }),
		);
		expect(
			await view.findByText(
				"VMID 105 is taken. Pick another or leave the suggested one.",
			),
		).toBeVisible();
		expect(
			view.getByRole("button", { name: "Restore as CT 105" }),
		).toBeDisabled();
	});

	it("keyboard: the arrow skips the disabled radio, Enter in the VMID field starts", async () => {
		const { mock, user, dialog } = await open();
		const view = within(dialog);
		const field = await view.findByRole("textbox", { name: "VMID" });
		const asNew = view.getByRole("radio", { name: "New container" });
		asNew.focus();
		await user.keyboard("{ArrowDown}");
		expect(asNew).toHaveFocus();
		await user.click(field);
		await user.keyboard("{Enter}");
		await waitFor(() => expect(mock.posts).toHaveLength(1));
	});

	it("Escape closes the dialog and focus returns to the button", async () => {
		const { user, button } = await open();
		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(button).toHaveFocus();
	});

	it("an unsigned snapshot needs confirming and cannot replace", async () => {
		const { mock, user, dialog } = await open({
			snapshot: { ...ctSnapshot, verified_by: null },
			targets: guestTargets(replaceable()),
		});
		const view = within(dialog);
		const start = await view.findByRole("button", {
			name: "Restore as CT 105",
		});
		expect(start).toBeDisabled();
		expect(replaceRadio(dialog)).toBeDisabled();
		await user.click(
			view.getByRole("checkbox", {
				name: "Restore this unsigned snapshot as a new container",
			}),
		);
		await user.click(start);
		await waitFor(() => expect(mock.posts).toHaveLength(1));
		expect(mock.posts[0]).toMatchObject({ accept_unverified: true });
	});
});
