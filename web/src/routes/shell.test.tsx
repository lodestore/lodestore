import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { copy } from "@/copy";
import { renderApp, useSignedIn } from "@/test/app";

async function renderAt(path: string) {
	renderApp(path);
	await screen.findByRole("heading", { level: 1 });
}

describe("app shell", () => {
	beforeEach(() => useSignedIn());

	it.each([
		["/overview", "See every backup at a glance"],
		["/policies", "Decide what gets backed up, and when"],
		["/jobs", "Follow every backup as it runs"],
		["/restore", "Restore any file from any snapshot"],
	])("shows an honest preview on %s", async (path, title) => {
		await renderAt(path);
		const h1 = screen.getByRole("heading", { level: 1, name: title });
		const main = h1.closest("section") as HTMLElement;
		expect(within(main).getByText(copy.preview.badge)).toBeInTheDocument();
		expect(within(main).getAllByRole("listitem")).toHaveLength(3);
		expect(within(main).getByText(copy.preview.footnote)).toBeInTheDocument();
		expect(within(main).queryAllByRole("button")).toHaveLength(0);
		expect(within(main).queryAllByRole("link")).toHaveLength(0);
	});

	it("swaps the preview illustration with the theme", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		const main = document.getElementById("main") as HTMLElement;
		expect(main.querySelectorAll("img")).toHaveLength(1);
		expect(main.querySelector("img")).toHaveAttribute(
			"src",
			expect.stringContaining("preview-overview.light"),
		);
		await user.click(screen.getByRole("button", { name: /admin/ }));
		await user.click(
			await screen.findByRole("menuitemradio", { name: "Dark" }),
		);
		expect(main.querySelectorAll("img")).toHaveLength(1);
		expect(main.querySelector("img")).toHaveAttribute(
			"src",
			expect.stringContaining("preview-overview.dark"),
		);
	});

	it("moves focus to the heading on every route change", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		await user.click(screen.getByRole("link", { name: "Policies" }));
		expect(
			await screen.findByRole("heading", {
				name: "Decide what gets backed up, and when",
			}),
		).toHaveFocus();
		await user.click(screen.getByRole("link", { name: "Jobs" }));
		expect(
			await screen.findByRole("heading", {
				name: "Follow every backup as it runs",
			}),
		).toHaveFocus();
	});

	it("keeps the skip link as the first tab stop on the first render", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		expect(screen.getByRole("heading", { level: 1 })).not.toHaveFocus();
		await user.tab();
		expect(
			screen.getByRole("link", { name: copy.shell.skipLink }),
		).toHaveFocus();
	});

	it("lists the six sections in order inside the main navigation", async () => {
		await renderAt("/overview");
		const nav = screen.getByRole("navigation", { name: "Main navigation" });
		const links = within(nav).getAllByRole("link");
		expect(links.map((l) => l.textContent)).toEqual([
			"Overview",
			"Hosts",
			"Policies",
			"Jobs",
			"Restore",
			"Settings",
		]);
		for (const group of ["Workspace", "Backup", "System"]) {
			expect(within(nav).getByText(group)).toBeInTheDocument();
		}
	});

	it("marks only the current section as the current page", async () => {
		await renderAt("/overview");
		const nav = screen.getByRole("navigation", { name: "Main navigation" });
		const current = within(nav)
			.getAllByRole("link")
			.filter((l) => l.getAttribute("aria-current") === "page");
		expect(current).toHaveLength(1);
		expect(current[0]).toHaveTextContent("Overview");
	});

	it("shows a Soon badge on the four stub sections", async () => {
		await renderAt("/overview");
		expect(screen.getAllByText("Soon")).toHaveLength(4);
	});

	it("makes the skip link the first tab stop and targets main", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		(document.activeElement as HTMLElement | null)?.blur();
		await user.tab();
		const skip = screen.getByRole("link", { name: "Skip to content" });
		expect(skip).toHaveFocus();
		expect(skip).toHaveAttribute("href", "#main");
		expect(document.getElementById("main")).toBeInTheDocument();
	});

	it("moves focus to a level-one heading after navigating", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		await user.click(screen.getByRole("link", { name: /^Hosts$/ }));
		await screen.findByRole("heading", { level: 1 });
		await vi.waitFor(() => {
			expect(document.activeElement?.tagName).toBe("H1");
		});
	});

	it("shows the user, version and theme options in the user menu", async () => {
		const user = userEvent.setup();
		await renderAt("/overview");
		await user.click(screen.getByRole("button", { name: /admin/ }));
		expect(await screen.findByText("v0.1.0")).toBeInTheDocument();
		expect(screen.getAllByText("admin").length).toBeGreaterThan(0);
		expect(screen.getAllByRole("menuitemradio")).toHaveLength(3);
		expect(
			screen.getByRole("menuitem", { name: "Sign out" }),
		).toBeInTheDocument();
	});
});
