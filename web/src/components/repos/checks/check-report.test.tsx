import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { toast } from "sonner";
import { describe, expect, it, vi } from "vitest";
import { CheckReport } from "@/components/repos/checks/check-report";
import { apiError } from "@/test/app";
import {
	checkRun,
	checkStatus,
	checksCard,
	failedPacksRun,
	levelStatus,
	openChecks,
	packsRun,
} from "@/test/checks";
import { server } from "@/test/server";

const row = (view: Awaited<ReturnType<typeof checksCard>>, name: string) =>
	within(
		view
			.getByText(name, { selector: "[data-slot=item-title]" })
			.closest("[data-slot=item]") as HTMLElement,
	);

describe("check report", () => {
	it("opens the failed report with findings", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs", { last: failedPacksRun(25) }),
				],
			}),
		);
		const view = await checksCard();
		expect(
			await view.findByText("Pack reading check found problems"),
		).toBeInTheDocument();
		expect(
			view.getByText("Some data may not restore. Open the report for details."),
		).toBeInTheDocument();
		expect(view.getByText("Found 25 problems.")).toBeInTheDocument();
		expect(view.getByText("Hide report")).toBeInTheDocument();
		const report = row(view, "Pack reading");
		expect(
			report.getAllByText(/blob hash mismatch in/, { selector: "span" }),
		).toHaveLength(20);
		expect(report.getByText("and 5 more.")).toBeInTheDocument();
		expect(
			report.getByRole("button", { name: "Download full report" }),
		).toBeInTheDocument();
		expect(report.getAllByText("abcd0").length).toBeGreaterThan(0);
	});

	it("shows a passed report on demand", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure", { last: checkRun() }),
					levelStatus("packs"),
				],
			}),
		);
		const view = await checksCard();
		const toggle = await view.findByRole("button", { name: "Show report" });
		expect(toggle).toHaveAttribute("aria-expanded", "false");
		expect(view.queryByText("Passed. No problems found.")).toBeNull();
		await userEvent.click(toggle);
		expect(
			await view.findByText("Passed. No problems found."),
		).toBeInTheDocument();
		expect(view.getByText("Missing packs")).toBeInTheDocument();
		expect(view.getByText("Snapshots checked")).toBeInTheDocument();
		expect(
			view.getByText("Missing packs").nextElementSibling,
		).toHaveTextContent("0");
	});

	it("shows pack coverage", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure"),
					levelStatus("packs", { last: packsRun() }),
				],
				coverage: { checked_30d_pct: 62.4, never_checked: 118, packs: 400 },
			}),
		);
		const view = await checksCard();
		await userEvent.click(
			await view.findByRole("button", { name: "Show report" }),
		);
		expect(await view.findByText("Checked in 30 days")).toBeInTheDocument();
		expect(view.getByText("62%")).toBeInTheDocument();
		expect(
			view.getByText("118 packs have never been checked."),
		).toBeInTheDocument();
		expect(view.getByText("5 GiB")).toBeInTheDocument();
	});

	it("groups failed levels in one alert", async () => {
		openChecks(
			checkStatus({
				levels: [
					levelStatus("structure", {
						last: checkRun({
							outcome: "problems",
							findings_total: 1,
							findings: [
								{ kind: "missing_pack", key: "packs/00/aa", detail: "missing" },
							],
						}),
					}),
					levelStatus("packs", { last: failedPacksRun(3) }),
				],
			}),
		);
		await checksCard();
		const alerts = await screen.findAllByRole("alert");
		expect(alerts).toHaveLength(1);
		const alert = within(alerts[0]);
		expect(
			alert.getByText("Structure check found problems"),
		).toBeInTheDocument();
		expect(
			alert.getByText("Pack reading check found problems"),
		).toBeInTheDocument();
	});

	it("titles a failed run with findings by its problem count", () => {
		const view = render(
			<CheckReport
				repositoryId="repo-1"
				run={failedPacksRun(2)}
				coverage={null}
			/>,
		);
		expect(screen.getByText("Found 2 problems.")).toBeInTheDocument();
		expect(screen.queryByText(/^Stopped:/)).not.toBeInTheDocument();
		view.rerender(
			<CheckReport
				repositoryId="repo-1"
				run={packsRun({
					state: "failed",
					outcome: "stopped",
					error: "bucket unreachable",
				})}
				coverage={null}
			/>,
		);
		expect(screen.getByText("Stopped: bucket unreachable")).toBeInTheDocument();
		view.rerender(
			<CheckReport
				repositoryId="repo-1"
				run={packsRun({ state: "failed", outcome: null, error: "boom" })}
				coverage={null}
			/>,
		);
		expect(screen.getByText("Stopped: boom")).toBeInTheDocument();
	});

	it("downloads the full report", async () => {
		const create = vi.fn(() => "blob:report");
		const revoke = vi.fn();
		Object.assign(URL, { createObjectURL: create, revokeObjectURL: revoke });
		const click = vi
			.spyOn(HTMLAnchorElement.prototype, "click")
			.mockImplementation(() => {});
		const error = vi.spyOn(toast, "error").mockImplementation(() => "");
		let fail = false;
		server.use(
			http.get("/api/v1/repositories/repo-1/checks/runs/:run/report", () =>
				fail
					? apiError(500, "internal", "boom")
					: HttpResponse.json({ run: {}, findings: [], notes: [] }),
			),
		);
		render(
			<CheckReport
				repositoryId="repo-1"
				run={failedPacksRun(25)}
				coverage={null}
			/>,
		);
		const button = screen.getByRole("button", { name: "Download full report" });
		await userEvent.click(button);
		await waitFor(() => expect(click).toHaveBeenCalledTimes(1));
		expect(create).toHaveBeenCalledTimes(1);
		fail = true;
		await userEvent.click(button);
		await waitFor(() =>
			expect(error).toHaveBeenCalledWith(
				"Couldn't download the report. Try again.",
			),
		);
		click.mockRestore();
		error.mockRestore();
	});
});
