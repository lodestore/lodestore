import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { copy } from "@/copy";
import { formatDateTime } from "@/lib/format";
import { renderApp, useSignedIn } from "@/test/app";
import { fleetHost, fleetOf, useFleet } from "@/test/fleet";

const text = copy.hostBackup.runs;
const AT = Date.UTC(2026, 5, 1, 12);

async function renderOverview() {
	renderApp("/overview");
	await screen.findByRole("heading", {
		level: 2,
		name: /attention$|protected$/,
	});
}

describe("recent runs", () => {
	beforeEach(() => useSignedIn());

	it("shows recent runs per host", async () => {
		useFleet(
			fleetOf([
				fleetHost("healthy", {
					id: "with-runs",
					hostname: "with-runs.example.com",
					recent_runs: [
						{ finished_at: AT, outcome: "succeeded", policy: "daily" },
						{ finished_at: AT + 1000, outcome: "failed", policy: "daily" },
						{ finished_at: AT + 2000, outcome: "skipped", policy: null },
					],
				}),
				fleetHost("not_protected", { id: "no-runs" }),
			]),
		);
		await renderOverview();

		const strip = await screen.findByRole("list", { name: text.label });
		const items = within(strip).getAllByRole("listitem");
		expect(items).toHaveLength(3);

		const failed = `Failed · ${formatDateTime(AT + 1000)} · daily`;
		expect(items[1]).toHaveAttribute("title", failed);
		expect(within(items[1]).getByText(failed)).toHaveClass("sr-only");
		expect(items[1]).toHaveClass("h-4");
		expect(items[0]).toHaveClass("h-2");
		expect(items[2]).toHaveAttribute(
			"title",
			`Skipped · ${formatDateTime(AT + 2000)}`,
		);

		const row = document.querySelector<HTMLElement>(
			'[data-host-row="no-runs"]',
		) as HTMLElement;
		expect(within(row).queryByRole("list", { name: text.label })).toBeNull();

		expect(screen.getByText(text.newestRight)).toBeInTheDocument();
		for (const word of Object.values(text.outcomes)) {
			expect(screen.getAllByText(word).length).toBeGreaterThan(0);
		}
	});

	it("hides the legend when no host has runs", async () => {
		useFleet(fleetOf([fleetHost("healthy")]));
		await renderOverview();
		await screen.findByText("healthy.example.com");

		expect(screen.queryByText(text.newestRight)).toBeNull();
	});
});
