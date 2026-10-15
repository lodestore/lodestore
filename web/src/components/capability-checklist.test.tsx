import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import {
	CapabilityChecklist,
	CHECK_IDS,
	type CheckRow,
	type CheckStatus,
} from "@/components/capability-checklist";

const rowsOf = (...statuses: CheckStatus[]): CheckRow[] =>
	CHECK_IDS.map((id, i) => ({ id, status: statuses[i] ?? "pass" }));

describe("CapabilityChecklist summary", () => {
	it("counts passed, warning and failed rows", () => {
		render(
			<CapabilityChecklist
				rows={rowsOf("pass", "pass", "pass", "warn", "fail")}
			/>,
		);
		expect(
			screen.getByText("3 passed, 1 with warnings, 1 failed"),
		).toBeInTheDocument();
	});

	it("drops zero counts", () => {
		render(<CapabilityChecklist rows={rowsOf()} />);
		expect(screen.getByText("5 passed")).toBeInTheDocument();
	});
});

describe("CapabilityChecklist rows", () => {
	it("tags only Object Lock as recommended", () => {
		render(<CapabilityChecklist rows={rowsOf()} />);
		const items = screen.getAllByRole("listitem");
		expect(items).toHaveLength(5);
		expect(screen.getAllByText("Recommended")).toHaveLength(1);
		expect(within(items[4]).getByText("Recommended")).toBeInTheDocument();
	});

	it("keeps a data-status on every row and the polite live region", () => {
		const { container } = render(
			<CapabilityChecklist rows={rowsOf("pass", "warn", "fail")} />,
		);
		expect(
			screen.getAllByRole("listitem").map((r) => r.dataset.status),
		).toEqual(["pass", "warn", "fail", "pass", "pass"]);
		expect(container.querySelector('[aria-live="polite"]')).not.toBeNull();
	});

	it("opens the explanation for a failure and closes it for a warning", async () => {
		const user = userEvent.setup();
		render(<CapabilityChecklist rows={rowsOf("pass", "warn", "fail")} />);
		const [, warn, fail] = screen.getAllByRole("listitem");
		const warnToggle = within(warn).getByRole("button", {
			name: "Why this matters",
		});
		const failToggle = within(fail).getByRole("button", {
			name: "Why this matters",
		});
		expect(warnToggle).toHaveAttribute("aria-expanded", "false");
		expect(failToggle).toHaveAttribute("aria-expanded", "true");

		await user.click(warnToggle);
		expect(warnToggle).toHaveAttribute("aria-expanded", "true");
	});

	it("offers no explanation for passing rows", () => {
		render(<CapabilityChecklist rows={rowsOf()} />);
		expect(
			screen.queryByRole("button", { name: "Why this matters" }),
		).toBeNull();
	});

	it("staggers the rows by 80 ms", () => {
		render(<CapabilityChecklist rows={rowsOf()} />);
		expect(
			screen.getAllByRole("listitem").map((r) => r.style.animationDelay),
		).toEqual(["0ms", "80ms", "160ms", "240ms", "320ms"]);
	});
});

describe("CapabilityChecklist stale result", () => {
	it("dims the list and shows the Outdated badge", () => {
		const { container } = render(<CapabilityChecklist rows={rowsOf()} stale />);
		expect(container.querySelector('[data-slot="item-group"]')).toHaveClass(
			"opacity-60",
		);
		expect(container.querySelector('[data-slot="badge"]')?.textContent).toBe(
			"Outdated",
		);
	});

	it("is not dimmed when fresh", () => {
		const { container } = render(<CapabilityChecklist rows={rowsOf()} />);
		expect(container.querySelector('[data-slot="item-group"]')).not.toHaveClass(
			"opacity-60",
		);
		expect(screen.queryByText("Outdated")).toBeNull();
	});
});
