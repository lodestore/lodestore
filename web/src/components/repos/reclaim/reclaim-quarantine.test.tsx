import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { GcQuarantineDto } from "@/api/gen/model";
import { apiError } from "@/test/app";
import { card, gcStatus, openRepository } from "@/test/gc";
import { server } from "@/test/server";

const entry = (
	id: string,
	over: Partial<GcQuarantineDto> = {},
): GcQuarantineDto => ({
	snapshot_id: id,
	key: `snapshots/${id}`,
	reason: "the root tree is missing",
	confirmable: true,
	detected_at: Date.now() - 3_600_000,
	confirmed_at: null,
	confirmed_by: null,
	...over,
});

const EXCLUDE = (id: string) =>
	`/api/v1/repositories/repo-1/gc/quarantine/${id}/exclude`;

describe("quarantine", () => {
	it("shows no block without entries or with only excluded ones", async () => {
		openRepository(
			gcStatus({
				quarantine: [
					entry("aa", { confirmed_at: Date.now(), confirmed_by: "admin" }),
				],
			}),
		);
		const view = await card();
		await view.findByText("Run cleanup");
		expect(view.queryByText(/Cleanup is stopped/)).not.toBeInTheDocument();
	});

	it("lists entries; only a confirmable one offers exclusion", async () => {
		openRepository(
			gcStatus({
				quarantine: [entry("aa"), entry("bb", { confirmable: false })],
			}),
		);
		const view = await card();
		expect(
			await view.findByText("Cleanup is stopped: 2 snapshots cannot be read"),
		).toBeInTheDocument();
		const rows = view.getAllByRole("listitem");
		expect(rows[0]).toHaveTextContent(
			"snapshots/aa · the root tree is missing",
		);
		expect(rows[1]).toHaveTextContent("snapshots/bb");
		expect(
			view.getAllByRole("button", { name: "Exclude from cleanup…" }),
		).toHaveLength(1);
		expect(view.getByText(/belongs to a backup job/)).toBeInTheDocument();
	});

	it("names a single entry in the singular", async () => {
		openRepository(gcStatus({ quarantine: [entry("aa")] }));
		const view = await card();
		expect(
			await view.findByText("Cleanup is stopped: 1 snapshot cannot be read"),
		).toBeInTheDocument();
	});

	it("asks first, then excludes and the block disappears", async () => {
		const current = openRepository(gcStatus({ quarantine: [entry("aa")] }));
		let posted = 0;
		server.use(
			http.post(EXCLUDE("aa"), () => {
				posted += 1;
				current.status = gcStatus();
				return HttpResponse.json(current.status);
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Exclude from cleanup…" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		expect(
			within(dialog).getByText(/may delete what only this snapshot uses/),
		).toBeInTheDocument();
		expect(posted).toBe(0);
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Exclude snapshot" }),
		);
		expect(
			await screen.findByText(
				"The snapshot is excluded. Run cleanup again to continue.",
			),
		).toBeInTheDocument();
		expect(posted).toBe(1);
		await waitFor(() =>
			expect(view.queryByText(/Cleanup is stopped/)).not.toBeInTheDocument(),
		);
	});

	it("cancel sends nothing", async () => {
		openRepository(gcStatus({ quarantine: [entry("aa")] }));
		let posted = 0;
		server.use(
			http.post(EXCLUDE("aa"), () => {
				posted += 1;
				return HttpResponse.json(gcStatus());
			}),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Exclude from cleanup…" }),
		);
		await userEvent.click(
			within(await screen.findByRole("alertdialog")).getByRole("button", {
				name: "Cancel",
			}),
		);
		await waitFor(() =>
			expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument(),
		);
		expect(posted).toBe(0);
	});

	it("keeps the dialog open and explains a 409", async () => {
		openRepository(gcStatus({ quarantine: [entry("aa")] }));
		server.use(
			http.post(EXCLUDE("aa"), () =>
				apiError(409, "conflict", "A backup job points at this snapshot."),
			),
		);
		const view = await card();
		await userEvent.click(
			await view.findByRole("button", { name: "Exclude from cleanup…" }),
		);
		const dialog = await screen.findByRole("alertdialog");
		await userEvent.click(
			within(dialog).getByRole("button", { name: "Exclude snapshot" }),
		);
		expect(
			await within(dialog).findByText(
				"Couldn't exclude the snapshot. A backup job points at this snapshot. Try again.",
			),
		).toBeInTheDocument();
		expect(screen.getByRole("alertdialog")).toBeInTheDocument();
	});
});
