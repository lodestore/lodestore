import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { ReclaimReport } from "@/components/repos/reclaim/reclaim-report";
import { apiError, renderApp, repository, useSignedIn } from "@/test/app";
import {
	card,
	GIB,
	gcStatus,
	openRepository,
	gcReport as report,
} from "@/test/gc";
import { server } from "@/test/server";

describe("free up space card", () => {
	it("renders off state with dry run available", async () => {
		openRepository();
		const view = await card();
		expect(await view.findByText("Cleanup is off")).toBeInTheDocument();
		expect(
			view.getByText("Nothing is deleted from this repository."),
		).toBeInTheDocument();
		expect(view.getByText("Beta")).toBeInTheDocument();
		expect(view.getByRole("button", { name: "Run cleanup" })).toBeDisabled();
		expect(view.getByRole("button", { name: "Dry run" })).toBeEnabled();
		expect(
			view.getByText(
				"Turn on scheduled cleanup to run it by hand. Dry run is always available.",
			),
		).toBeInTheDocument();
		expect(
			view.getByText(
				"No report yet. Run a dry run to see how much space cleanup would free.",
			),
		).toBeInTheDocument();
	});

	it("dry run shows would free report", async () => {
		const current = openRepository();
		server.use(
			http.post("/api/v1/repositories/repo-1/gc/dry-run", () => {
				current.status = gcStatus({
					last_dry_run: report({
						bytes_freed: 1.5 * GIB,
						packs_deleted: 12,
						packs_repacked: 4,
						repack_transfer_bytes: 2 * GIB,
					}),
				});
				return HttpResponse.json({ cycle_id: "cycle-1" }, { status: 202 });
			}),
		);
		const view = await card();
		await userEvent.click(await view.findByRole("button", { name: "Dry run" }));
		expect(await view.findByText("Would free 1.5 GiB.")).toBeInTheDocument();
		expect(view.getByText("Last dry run")).toBeInTheDocument();
		expect(view.getByText("To delete")).toBeInTheDocument();
		expect(view.getByText("To repack")).toBeInTheDocument();
		expect(view.getByText("Repack transfer")).toBeInTheDocument();
		expect(view.getByText("2 GiB")).toBeInTheDocument();
		expect(view.queryByText("Held by Object Lock")).not.toBeInTheDocument();
	});

	it("says nothing to free without hiding the facts", async () => {
		openRepository(
			gcStatus({
				last_dry_run: report({ bytes_freed: 0, packs_deleted: 0 }),
			}),
		);
		const view = await card();
		expect(
			await view.findByText("Nothing to free right now."),
		).toBeInTheDocument();
		expect(view.getByText("To delete")).toBeInTheDocument();
		expect(view.queryByText("Repack transfer")).not.toBeInTheDocument();
	});

	it("report tabs only with both reports", async () => {
		const both = gcStatus({
			last_cleanup: report({
				cycle_id: "c-real",
				dry_run: false,
				bytes_freed: 2 * GIB,
				finished_at: Date.now() - 3_600_000,
			}),
			last_dry_run: report({
				finished_at: Date.now() - 86_400_000,
				bytes_freed: 5 * GIB,
			}),
		});
		openRepository(both);
		const view = await card();
		expect(
			await view.findByRole("tab", { name: "Last cleanup" }),
		).toBeVisible();
		expect(view.getByRole("tab", { name: "Last dry run" })).toBeVisible();
		expect(view.getByText("Freed 2 GiB.")).toBeInTheDocument();
		expect(view.getByText(/took 1 min 35 s/)).toBeInTheDocument();
		await userEvent.click(view.getByRole("tab", { name: "Last dry run" }));
		expect(await view.findByText("Would free 5 GiB.")).toBeInTheDocument();
	});

	it("shows a plain heading for a single report", async () => {
		openRepository(gcStatus({ last_dry_run: report() }));
		const view = await card();
		expect(await view.findByText("Last dry run")).toBeInTheDocument();
		expect(view.queryByRole("tab")).not.toBeInTheDocument();
	});

	it("object lock fact only when locked", async () => {
		openRepository(
			gcStatus({
				has_object_lock: true,
				last_cleanup: report({
					dry_run: false,
					bytes_freed: 12 * GIB,
					locked_bytes: 3 * GIB,
					locked_until: Date.UTC(2026, 9, 24, 12),
				}),
			}),
		);
		const view = await card();
		expect(
			await view.findByText("Freed 12 GiB. 3 GiB more is held by Object Lock."),
		).toBeInTheDocument();
		expect(view.getByText("Held by Object Lock")).toBeInTheDocument();
		expect(view.getByText(/Frees after Oct 24, 2026/)).toBeInTheDocument();
	});

	it("says when the object lock release date is unknown", async () => {
		openRepository(
			gcStatus({
				last_cleanup: report({
					dry_run: false,
					locked_bytes: GIB,
					locked_until_unknown: true,
				}),
			}),
		);
		const view = await card();
		expect(await view.findByText("Release date unknown")).toBeInTheDocument();
	});

	it("load error shows retry", async () => {
		useSignedIn();
		let fail = true;
		server.use(
			http.get("/api/v1/repositories/repo-1", () =>
				HttpResponse.json({
					...repository,
					id: "repo-1",
					location: { backend: "s3", options: { bucket: "b", prefix: "p" } },
					checks: [],
				}),
			),
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "" }),
			),
			http.get("/api/v1/repositories/repo-1/gc", () =>
				fail
					? apiError(500, "internal", "boom")
					: HttpResponse.json(gcStatus()),
			),
		);
		renderApp("/settings/repositories/repo-1");
		const view = await card();
		expect(
			await view.findByText(
				"Couldn't load cleanup status. Check that the server is reachable, then retry.",
			),
		).toBeInTheDocument();
		fail = false;
		await userEvent.click(view.getByRole("button", { name: "Retry loading" }));
		await waitFor(() =>
			expect(view.getByText("Cleanup is off")).toBeInTheDocument(),
		);
	});

	it("focuses the card heading for the anchor", async () => {
		useSignedIn();
		server.use(
			http.get("/api/v1/repositories/repo-1", () =>
				HttpResponse.json({
					...repository,
					id: "repo-1",
					location: { backend: "s3", options: { bucket: "b", prefix: "p" } },
					checks: [],
				}),
			),
			http.get("/api/v1/agent-policy", () =>
				HttpResponse.json({ iam_json: "{}", minio_commands: "" }),
			),
			http.get("/api/v1/repositories/repo-1/gc", () =>
				HttpResponse.json(gcStatus()),
			),
		);
		renderApp("/settings/repositories/repo-1#free-up-space");
		const heading = await screen.findByRole("heading", {
			name: "Free up space",
		});
		await waitFor(() => expect(heading).toHaveFocus());
	});

	it("opens a newer report when it arrives, but keeps a tab the user picked", async () => {
		const cleanup = report({
			cycle_id: "c-real",
			dry_run: false,
			bytes_freed: 2 * GIB,
			finished_at: Date.now() - 3_600_000,
		});
		const dryRun = report({
			cycle_id: "d-1",
			finished_at: Date.now() - 86_400_000,
			bytes_freed: 5 * GIB,
		});
		const view = render(
			<ReclaimReport repositoryId="repo-1" cleanup={cleanup} dryRun={dryRun} />,
		);
		expect(screen.getByRole("tab", { name: "Last cleanup" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
		const newer = report({
			cycle_id: "d-2",
			finished_at: Date.now(),
			bytes_freed: 7 * GIB,
		});
		view.rerender(
			<ReclaimReport repositoryId="repo-1" cleanup={cleanup} dryRun={newer} />,
		);
		expect(screen.getByRole("tab", { name: "Last dry run" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
		expect(screen.getByText("Would free 7 GiB.")).toBeInTheDocument();
		await userEvent.click(screen.getByRole("tab", { name: "Last cleanup" }));
		view.rerender(
			<ReclaimReport
				repositoryId="repo-1"
				cleanup={cleanup}
				dryRun={{ ...newer }}
				busy
			/>,
		);
		expect(screen.getByRole("tab", { name: "Last cleanup" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
	});
});
