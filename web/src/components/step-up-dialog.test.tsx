import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { apiError, renderApp, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const SECRET = "lsk_Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZg";

type StepUpReply = () => Response;

function mockFlow(
	options: { stepUp?: StepUpReply; repeatChallenge?: boolean } = {},
) {
	const calls = { created: 0, stepUp: [] as unknown[] };
	server.use(
		http.get("/api/v1/tokens", () => HttpResponse.json([])),
		http.post("/api/v1/tokens", () => {
			calls.created += 1;
			if (calls.created === 1 || options.repeatChallenge) {
				return apiError(403, "step_up_required", "password required");
			}
			return HttpResponse.json(
				{
					token: SECRET,
					item: {
						id: "id-ci",
						name: "ci",
						prefix: "lsk_a1b2c3",
						scopes: ["policies:read"],
						created_at: Date.now(),
						created_by: "admin",
						expires_at: null,
						last_used_at: null,
						revoked_at: null,
					},
				},
				{ status: 201 },
			);
		}),
		http.post("/api/v1/auth/step-up", async ({ request }) => {
			calls.stepUp.push(await request.json());
			return options.stepUp?.() ?? new HttpResponse(null, { status: 204 });
		}),
	);
	return calls;
}

async function submitTokenForm() {
	useSignedIn();
	renderApp("/settings/tokens");
	const user = userEvent.setup();
	await screen.findByRole("heading", {
		name: "API tokens",
		level: 1,
		hidden: true,
	});
	await user.click(screen.getAllByRole("button", { name: "Create token" })[0]);
	const dialog = await screen.findByRole("dialog", {
		name: "Create API token",
	});
	await user.type(within(dialog).getByLabelText("Name"), "ci");
	await user.click(
		within(dialog).getByRole("checkbox", { name: "policies:read" }),
	);
	await user.click(
		within(dialog).getByRole("button", { name: "Create token" }),
	);
	return user;
}

const stepUpDialog = () =>
	screen.findByRole("dialog", { name: "Confirm your password" });

describe("step-up dialog", () => {
	it("asks for the password and repeats the request once", async () => {
		const calls = mockFlow();
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.type(
			within(dialog).getByLabelText("Password"),
			"hunter2hunter2",
		);
		await user.click(within(dialog).getByRole("button", { name: "Confirm" }));

		await screen.findByRole("dialog", { name: "Token created" });
		expect(calls.created).toBe(2);
		expect(calls.stepUp).toEqual([{ password: "hunter2hunter2" }]);
		expect(
			screen.queryByRole("dialog", { name: "Confirm your password" }),
		).toBeNull();
	});

	it("cancel rejects the original request without calling step-up", async () => {
		const calls = mockFlow();
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		const form = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		expect(await within(form).findByRole("alert")).toHaveTextContent(
			"Couldn't create the token. Password required. Try again.",
		);
		expect(calls.created).toBe(1);
		expect(calls.stepUp).toEqual([]);
	});

	it("Esc closes only the step-up dialog", async () => {
		mockFlow();
		const user = await submitTokenForm();
		await stepUpDialog();
		await user.keyboard("{Escape}");

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(
			screen.getByRole("dialog", { name: "Create API token" }),
		).toBeInTheDocument();
	});

	it("shows a wrong password under the field and stays open", async () => {
		mockFlow({
			stepUp: () => apiError(422, "wrong_password", "wrong password"),
		});
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.type(within(dialog).getByLabelText("Password"), "nope");
		await user.click(within(dialog).getByRole("button", { name: "Confirm" }));

		expect(
			await within(dialog).findByText("Password is wrong."),
		).toBeInTheDocument();
		expect(within(dialog).getByLabelText("Password")).toHaveValue("");
	});

	it("tells the user the server is busy", async () => {
		mockFlow({
			stepUp: () =>
				HttpResponse.json(
					{ code: "busy", message: "busy" },
					{ status: 503, headers: { "Retry-After": "2" } },
				),
		});
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.type(within(dialog).getByLabelText("Password"), "pw");
		await user.click(within(dialog).getByRole("button", { name: "Confirm" }));

		expect(
			await within(dialog).findByText(
				"The server is busy. Try again in 2 seconds.",
			),
		).toBeInTheDocument();
	});

	it("a second challenge on the repeat goes to the caller, not a second dialog", async () => {
		const calls = mockFlow({ repeatChallenge: true });
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.type(within(dialog).getByLabelText("Password"), "pw");
		await user.click(within(dialog).getByRole("button", { name: "Confirm" }));

		const form = await screen.findByRole("dialog", {
			name: "Create API token",
		});
		expect(await within(form).findByRole("alert")).toHaveTextContent(
			"Couldn't create the token. Password required. Try again.",
		);
		expect(
			screen.queryByRole("dialog", { name: "Confirm your password" }),
		).toBeNull();
		expect(calls.created).toBe(2);
		expect(calls.stepUp).toHaveLength(1);
	});

	it("401 from step-up closes the dialog and fails the request", async () => {
		const calls = mockFlow({
			stepUp: () => apiError(401, "unauthorized", "session ended"),
		});
		const user = await submitTokenForm();

		const dialog = await stepUpDialog();
		await user.type(within(dialog).getByLabelText("Password"), "pw");
		await user.click(within(dialog).getByRole("button", { name: "Confirm" }));

		await waitFor(() =>
			expect(
				screen.queryByRole("dialog", { name: "Confirm your password" }),
			).toBeNull(),
		);
		expect(calls.created).toBe(1);
	});
});
