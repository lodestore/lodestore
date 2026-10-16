import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { copy } from "@/copy";
import { renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

async function renderAt(path: string) {
	renderApp(path);
	await screen.findByRole("heading", { level: 1 });
}

describe("app shell", () => {
	beforeEach(() => useSignedIn());

	it("shows an honest preview on /overview", async () => {
		await renderAt("/overview");
		const h1 = screen.getByRole("heading", {
			level: 1,
			name: "See every backup at a glance",
		});
		const main = h1.closest("section") as HTMLElement;
		expect(within(main).getByText(copy.preview.badge)).toBeInTheDocument();
		expect(within(main).getAllByRole("listitem")).toHaveLength(3);
		expect(within(main).getByText(copy.preview.footnote)).toBeInTheDocument();
		expect(within(main).queryAllByRole("button")).toHaveLength(0);
		expect(within(main).queryAllByRole("link")).toHaveLength(0);
	});

	it("opens the working Restore section instead of a preview", async () => {
		server.use(http.get("/api/v1/restore/hosts", () => HttpResponse.json([])));
		await renderAt("/restore");
		const h1 = screen.getByRole("heading", { level: 1, name: "Restore" });
		expect(h1).toHaveAttribute("data-page-title");
		expect(
			await screen.findByText(copy.restore.empty.heading),
		).toBeInTheDocument();
		expect(screen.queryByText(copy.preview.badge)).not.toBeInTheDocument();
	});

	it.each(["/policies", "/jobs"])("opens the %s section", async (path) => {
		await renderAt(path);
		expect(document.querySelector("h1[data-page-title]")).toBeInTheDocument();
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
		for (const name of ["Policies", "Jobs"]) {
			await user.click(screen.getByRole("link", { name }));
			await vi.waitFor(() => {
				expect(document.querySelector("h1[data-page-title]")).toHaveFocus();
			});
		}
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

	it("shows a Soon badge on the stub section", async () => {
		await renderAt("/overview");
		expect(screen.getAllByText("Soon")).toHaveLength(1);
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
		const reference = screen.getByRole("menuitem", { name: "API reference" });
		expect(reference).toHaveAttribute("href", "/api/docs");
		expect(reference).toHaveAttribute("target", "_blank");
	});
});
