import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { StatusBadge } from "@/components/status-badge";
import { copy } from "@/copy";

const jobStatuses = Object.keys(
	copy.jobs.status,
) as (keyof typeof copy.jobs.status)[];

describe("StatusBadge job statuses", () => {
	it.each(jobStatuses)(
		"%s renders an icon, its word and data-status",
		(status) => {
			render(<StatusBadge status={status} />);
			const badge = screen
				.getByText(copy.jobs.status[status])
				.closest("[data-status]");
			expect(badge).toHaveAttribute("data-status", status);
			expect(badge?.querySelector("svg")).not.toBeNull();
		},
	);

	it("spins the running icon only when motion is allowed", () => {
		render(<StatusBadge status="running" />);
		const badge = screen
			.getByText(copy.jobs.status.running)
			.closest("[data-status]");
		expect(badge?.className).toContain("motion-safe:[&>svg]:animate-spin");
		expect(badge?.className).toContain("text-foreground");
	});
});
