import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { server } from "@/test/server";
import { newDraft } from "./draft";
import { RecoveryKeyStep } from "./recovery-key-step";

const PASSWORD = "correct horse battery";
const KEY = "lode-rk1-aaaa-bbbb-wxyz";

afterEach(() => {
	vi.restoreAllMocks();
	sessionStorage.clear();
});

function renderStep() {
	server.use(
		http.post("/api/v1/repositories/wizard", () =>
			HttpResponse.json(
				{ wizard_id: "w1", recovery_key: KEY, expires_at: Date.now() + 1e6 },
				{ status: 201 },
			),
		),
	);
	const user = userEvent.setup();
	render(
		<RecoveryKeyStep
			draft={newDraft()}
			onBack={() => {}}
			onCreated={() => {}}
		/>,
	);
	return user;
}

function section(name: string) {
	const heading = screen.getByRole("heading", {
		name: new RegExp(`^${name}( completed)?$`),
	});
	const card = heading.parentElement;
	if (!card) throw new Error("section without a card");
	return { heading, card };
}

describe("RecoveryKeyStep", () => {
	it("shows three numbered sections in order", async () => {
		renderStep();
		await screen.findByText(KEY);
		const names = screen
			.getAllByRole("heading", { level: 3 })
			.map((h) => h.textContent);
		expect(names).toEqual([
			"1. Save the key",
			"2. Choose a password",
			"3. Confirm you saved it",
		]);
	});

	it("captions the last group of the key", async () => {
		renderStep();
		await screen.findByText(KEY);
		const caption = screen.getByText("Last group");
		expect(
			within(caption.parentElement as HTMLElement).getByText("wxyz"),
		).toBeInTheDocument();
	});

	it("shows the lode-rk1 prefix apart from the four-column group grid", async () => {
		renderStep();
		await screen.findByText(KEY);
		const prefix = screen.getByText("lode-rk1");
		const grid = screen.getByText("aaaa").closest("[aria-hidden]");
		expect(grid).toHaveClass("grid-cols-4");
		expect(grid).not.toContainElement(prefix);
		expect(screen.queryByText("lode")).toBeNull();
		expect(screen.queryByText("rk1")).toBeNull();
	});

	it("names what is left next to the disabled Create repository", async () => {
		const user = renderStep();
		await screen.findByText(KEY);
		const create = screen.getByRole("button", { name: "Create repository" });
		expect(create).toHaveAccessibleDescription(
			"Opens the repository together with the recovery key. At least 12 characters.",
		);

		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		expect(create).toHaveAccessibleDescription(
			"Repeat the password to continue.",
		);

		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		expect(create).toHaveAccessibleDescription(
			"Type the last group of the key to continue.",
		);

		await user.type(
			screen.getByLabelText("Type the last group of the key"),
			"wxyz",
		);
		expect(create).toBeEnabled();
		expect(create).not.toHaveAccessibleDescription();
		expect(screen.queryByText(/to continue\.$/)).toBeNull();
	});

	it("notes Copied and Downloaded without unlocking Create repository", async () => {
		URL.createObjectURL = vi.fn(() => "blob:key");
		URL.revokeObjectURL = vi.fn();
		vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
		const user = renderStep();
		await screen.findByText(KEY);
		const create = screen.getByRole("button", { name: "Create repository" });

		await user.click(screen.getByRole("button", { name: "Copy recovery key" }));
		expect(await screen.findByText("Copied")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Download .txt" }));
		expect(screen.getByText("Downloaded")).toBeInTheDocument();
		expect(URL.revokeObjectURL).not.toHaveBeenCalled();
		expect(create).toBeDisabled();
	});

	it("checks the password section once the rule and the confirmation pass", async () => {
		const user = renderStep();
		await screen.findByText(KEY);
		const password = section("2. Choose a password");
		expect(password.heading).not.toHaveAttribute("data-done");

		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		expect(password.heading).toHaveAttribute("data-done");
		expect(password.heading).toHaveAccessibleName(
			"2. Choose a password completed",
		);
		expect(password.card.querySelector("[data-slot=done]")).not.toBeNull();
		expect(section("1. Save the key").heading).not.toHaveAttribute("data-done");
	});

	it("checks the last group section and enables Create repository", async () => {
		const user = renderStep();
		await screen.findByText(KEY);
		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		await user.type(
			screen.getByLabelText("Type the last group of the key"),
			"WXYZ",
		);
		expect(section("3. Confirm you saved it").heading).toHaveAttribute(
			"data-done",
		);
		expect(
			screen.getByRole("button", { name: "Create repository" }),
		).toBeEnabled();
	});

	it("does not bring the key back when the draft changes after creating", async () => {
		function Harness() {
			const [draft, setDraft] = useState(newDraft());
			return (
				<RecoveryKeyStep
					draft={draft}
					onBack={() => {}}
					onCreated={() => setDraft((d) => ({ ...d }))}
				/>
			);
		}
		server.use(
			http.post("/api/v1/repositories/wizard", () =>
				HttpResponse.json(
					{ wizard_id: "w1", recovery_key: KEY, expires_at: Date.now() + 1e6 },
					{ status: 201 },
				),
			),
			http.post("/api/v1/repositories/wizard/w1/commit", () =>
				HttpResponse.json({ id: "repo-1", name: "main" }, { status: 201 }),
			),
		);
		const user = userEvent.setup();
		render(<Harness />);
		await screen.findByText(KEY);
		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		await user.type(
			screen.getByLabelText("Type the last group of the key"),
			"WXYZ",
		);
		await user.click(screen.getByRole("button", { name: "Create repository" }));

		await waitFor(() => expect(screen.queryByText(KEY)).toBeNull());
		await new Promise((resolve) => setTimeout(resolve, 50));
		expect(screen.queryByText(KEY)).toBeNull();
	});

	it("reports a wrong last group and keeps Create repository disabled", async () => {
		const user = renderStep();
		await screen.findByText(KEY);
		await user.type(screen.getByLabelText("Repository password"), PASSWORD);
		await user.type(screen.getByLabelText("Confirm password"), PASSWORD);
		await user.type(
			screen.getByLabelText("Type the last group of the key"),
			"nope",
		);
		expect(
			screen.getByText(
				"That does not match the last group of the key. Check the saved copy.",
			),
		).toBeInTheDocument();
		expect(section("3. Confirm you saved it").heading).not.toHaveAttribute(
			"data-done",
		);
		expect(
			screen.getByRole("button", { name: "Create repository" }),
		).toBeDisabled();
	});
});
