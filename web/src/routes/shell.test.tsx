import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";
import { renderApp, useSignedIn } from "@/test/app";

async function renderAt(path: string) {
	renderApp(path);
	await screen.findByRole("heading", { level: 1 });
}

describe("app shell", () => {
	beforeEach(() => useSignedIn());

	it("shows the stub heading on /overview", async () => {
		await renderAt("/overview");
		expect(
			screen.getByRole("heading", { name: "Overview is not available yet" }),
		).toBeInTheDocument();
	});

	it("moves focus to the heading on every route change", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		await user.click(screen.getByRole("link", { name: "Policies" }));
		expect(
			await screen.findByRole("heading", {
				name: "Policies is not available yet",
			}),
		).toHaveFocus();
		await user.click(screen.getByRole("link", { name: "Jobs" }));
		expect(
			await screen.findByRole("heading", {
				name: "Jobs is not available yet",
			}),
		).toHaveFocus();
	});

	it("lists the six sections in order", async () => {
		await renderAt("/overview");
		const links = screen.getAllByRole("link");
		expect(links.map((l) => l.textContent)).toEqual([
			"Overview",
			"Hosts",
			"Policies",
			"Jobs",
			"Restore",
			"Settings",
		]);
	});

	it("marks only the current section as the current page", async () => {
		await renderAt("/overview");
		expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute(
			"aria-current",
			"page",
		);
		expect(screen.getByRole("link", { name: "Hosts" })).not.toHaveAttribute(
			"aria-current",
		);
	});

	it("shows a Soon badge on the four stub sections", async () => {
		await renderAt("/overview");
		expect(screen.getAllByText("Soon")).toHaveLength(4);
	});
});
