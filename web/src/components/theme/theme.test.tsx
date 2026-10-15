import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";
import { renderApp, useSignedIn } from "@/test/app";
import { setPrefersDark } from "@/test/media";

const isDark = () => document.documentElement.classList.contains("dark");

async function openUserMenu() {
	const user = userEvent.setup();
	await user.click(await screen.findByRole("button", { name: "admin" }));
	return user;
}

describe("theme", () => {
	beforeEach(() => useSignedIn());

	it("applies and stores the choice from the user menu", async () => {
		renderApp("/overview");
		const user = await openUserMenu();
		await user.click(screen.getByRole("menuitemradio", { name: "Dark" }));
		expect(isDark()).toBe(true);
		expect(localStorage.getItem("lodestore.theme")).toBe("dark");
		await user.click(await screen.findByRole("button", { name: "admin" }));
		expect(screen.getByRole("menuitemradio", { name: "Dark" })).toHaveAttribute(
			"aria-checked",
			"true",
		);
	});

	it("restores the stored choice on mount", async () => {
		localStorage.setItem("lodestore.theme", "light");
		setPrefersDark(true);
		renderApp("/overview");
		await openUserMenu();
		expect(isDark()).toBe(false);
		expect(
			screen.getByRole("menuitemradio", { name: "Light" }),
		).toHaveAttribute("aria-checked", "true");
	});

	it("follows the system preference while set to system", async () => {
		renderApp("/overview");
		await screen.findByRole("button", { name: "admin" });
		expect(isDark()).toBe(false);
		act(() => setPrefersDark(true));
		expect(isDark()).toBe(true);
		act(() => setPrefersDark(false));
		expect(isDark()).toBe(false);
	});

	it("treats a tampered stored value as system", async () => {
		localStorage.setItem("lodestore.theme", "<script>");
		renderApp("/overview");
		await openUserMenu();
		expect(
			screen.getByRole("menuitemradio", { name: "System" }),
		).toHaveAttribute("aria-checked", "true");
	});

	it("writes nothing to browser storage without an explicit choice", async () => {
		renderApp("/overview");
		await screen.findByRole("button", { name: "admin" });
		expect(localStorage.length).toBe(0);
	});
});
