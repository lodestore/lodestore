import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { renderApp, useSignedIn } from "@/test/app";
import { fleetOverview, useFleet } from "@/test/fleet";
import { channel, mockChannels, openNotifications } from "@/test/notifications";
import { server } from "@/test/server";

const NEW = "/settings/notifications/new";

const row = (name: string) => {
	const tr = screen.getByText(name).closest("tr");
	if (!tr) throw new Error(`row ${name} missing`);
	return tr;
};

describe("notifications: tabs and list", () => {
	it("shows four settings tabs", async () => {
		mockChannels([]);
		await openNotifications();
		const tabs = screen.getByRole("tablist", { name: "Settings sections" });
		expect(
			Array.from(tabs.querySelectorAll("a")).map((a) => a.textContent),
		).toEqual(["Repositories", "Notifications", "API tokens", "User"]);
		expect(
			within(tabs).getByRole("tab", { name: "Notifications" }),
		).toHaveAttribute("aria-selected", "true");
	});

	it("lists channels with their last delivery", async () => {
		mockChannels([
			channel("pager", {
				last_delivery: { at: Date.now() - 3_600_000, ok: true, error: null },
			}),
			channel("from-yaml", {
				source: "config",
				alert_types: ["backup_failed", "agent_offline"],
				last_delivery: {
					at: Date.now() - 60_000,
					ok: false,
					error: "connect refused",
				},
			}),
			channel("quiet"),
		]);
		await openNotifications();

		await screen.findByText("pager");
		expect(within(row("pager")).getByText("Sent")).toBeInTheDocument();
		expect(within(row("pager")).getByText("1 hour ago")).toBeInTheDocument();
		expect(within(row("pager")).getByText("All types")).toBeInTheDocument();
		expect(within(row("pager")).getByText("Webhook")).toBeInTheDocument();

		expect(
			within(row("from-yaml")).getByText("From server config"),
		).toBeInTheDocument();
		expect(within(row("from-yaml")).getByText("Failed")).toBeInTheDocument();
		expect(within(row("from-yaml")).getByText("2 of 8")).toBeInTheDocument();
		expect(within(row("quiet")).getByText("Never sent")).toBeInTheDocument();
	});
});

describe("notifications: webhook channel", () => {
	it("creates a webhook channel", async () => {
		const calls = mockChannels([]);
		const user = userEvent.setup();
		await openNotifications(NEW, "Add channel");

		await user.type(await screen.findByLabelText("Name"), "alertmanager");
		await user.type(
			screen.getByLabelText("URL"),
			"https://am.example.com/hook",
		);
		await user.click(screen.getByRole("button", { name: "Save channel" }));

		await waitFor(() => expect(calls.created).toHaveLength(1));
		expect(calls.created[0]).toMatchObject({
			type: "webhook",
			name: "alertmanager",
			enabled: true,
			settings: { url: "https://am.example.com/hook" },
			secret: null,
		});
		expect(calls.created[0].alert_types).toHaveLength(8);
		expect(
			(await screen.findAllByText("Channel saved.")).length,
		).toBeGreaterThan(0);
		expect(
			await screen.findByRole("heading", { name: "Notifications", level: 1 }),
		).toBeInTheDocument();
		expect(await screen.findByText("alertmanager")).toBeInTheDocument();
	});
});

async function chooseType(
	user: ReturnType<typeof userEvent.setup>,
	name: string,
) {
	await user.click(await screen.findByRole("combobox", { name: "Type" }));
	await user.click(await screen.findByRole("option", { name }));
}

function mockAdminEmail(email: string | null) {
	server.use(
		http.get("/api/v1/account", () =>
			HttpResponse.json({ username: "admin", email }),
		),
	);
}

describe("notifications: channel types", () => {
	it("switches channel types", async () => {
		mockChannels([]);
		mockAdminEmail("ops@example.com");
		const user = userEvent.setup();
		await openNotifications(NEW, "Add channel");

		expect(await screen.findByLabelText("URL")).toBeInTheDocument();
		await user.type(screen.getByLabelText("Name"), "first");

		await chooseType(user, "Telegram");
		expect(screen.getByLabelText("Bot token")).toHaveAttribute(
			"type",
			"password",
		);
		expect(screen.getByLabelText("Chat ID")).toBeInTheDocument();
		expect(
			screen.getByText("A number, or @channelname for public channels."),
		).toBeInTheDocument();
		expect(screen.queryByLabelText("URL")).toBeNull();
		await user.type(screen.getByLabelText("Chat ID"), "-1001");

		await chooseType(user, "Slack");
		expect(screen.getByLabelText("Webhook URL")).toBeInTheDocument();
		expect(
			screen.getByText(
				"Create an incoming webhook in Slack and paste its URL.",
			),
		).toBeInTheDocument();
		expect(screen.queryByLabelText("Chat ID")).toBeNull();

		await chooseType(user, "Email");
		expect(screen.getByLabelText("Port")).toHaveValue("587");
		expect(
			screen.getByRole("combobox", { name: "Security" }),
		).toHaveTextContent("STARTTLS");
		for (const label of [
			"Recipients",
			"SMTP host",
			"Username",
			"Password",
			"From",
		]) {
			expect(screen.getByLabelText(label)).toBeInTheDocument();
		}
		expect(
			await screen.findByText(
				"Leave empty to use the admin email (ops@example.com).",
			),
		).toBeInTheDocument();
		await user.click(screen.getByRole("combobox", { name: "Security" }));
		expect(
			screen.queryByRole("option", { name: /none|plain|unencrypted/i }),
		).toBeNull();
		expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
			"STARTTLS",
			"TLS",
		]);
		await user.keyboard("{Escape}");

		expect(screen.getByLabelText("Name")).toHaveValue("first");
		await chooseType(user, "Telegram");
		expect(screen.getByLabelText("Chat ID")).toHaveValue("");
	});

	it("validates channel fields", async () => {
		const calls = mockChannels([]);
		const user = userEvent.setup();
		await openNotifications(NEW, "Add channel");

		await chooseType(user, "Telegram");
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		expect(
			screen.getByText("Enter a name for the channel."),
		).toBeInTheDocument();
		expect(screen.getByText("Enter a bot token.")).toBeInTheDocument();
		expect(screen.getByText("Enter a chat ID.")).toBeInTheDocument();
		expect(screen.getByLabelText("Name")).toBeInvalid();
		expect(screen.getByLabelText("Name")).toHaveFocus();
		expect(calls.created).toEqual([]);

		await chooseType(user, "Email");
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		expect(
			screen.getByText(
				"Enter a recipient or set an admin email in Settings, User.",
			),
		).toBeInTheDocument();
		expect(screen.getByText("Enter the SMTP host.")).toBeInTheDocument();
		await user.clear(screen.getByLabelText("Port"));
		await user.type(screen.getByLabelText("Port"), "70000");
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		expect(
			screen.getByText("Enter a port from 1 to 65535."),
		).toBeInTheDocument();

		await user.click(screen.getByRole("checkbox", { name: "Backup failed" }));
		for (const label of [
			"No recent backup",
			"Agent offline",
			"Check failed",
			"Certificate expiring",
			"Agent update failed",
			"Snapshot quarantined",
			"Repository unavailable",
		]) {
			await user.click(screen.getByRole("checkbox", { name: label }));
		}
		expect(
			screen.getByText("Choose at least one alert type."),
		).toBeInTheDocument();

		await chooseType(user, "Slack");
		await user.type(screen.getByLabelText("Name"), "ops");
		await user.click(screen.getByRole("checkbox", { name: "Backup failed" }));
		await user.type(
			screen.getByLabelText("Webhook URL"),
			"https://hooks.slack.test/x",
		);
		server.use(
			http.post("/api/v1/notifications/channels", () =>
				HttpResponse.json(
					{
						errors: [
							{ path: "settings.url", message: "Slack does not accept it" },
						],
					},
					{ status: 422 },
				),
			),
		);
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		expect(await screen.findByText("Slack does not accept it")).toBeVisible();
		expect(screen.getByLabelText("Webhook URL")).toBeInvalid();
		expect(
			screen.getByText(/Couldn't save the channel\. Slack does not accept it/),
		).toBeInTheDocument();
	});
});

describe("notifications: secrets and tests", () => {
	it("replaces a saved secret", async () => {
		const calls = mockChannels([
			channel("pager", {
				type: "telegram",
				settings: { chat_id: "-100" },
				secret_set: true,
			}),
		]);
		const user = userEvent.setup();
		const { router } = await openNotifications(
			"/settings/notifications/id-pager",
			"pager",
		);

		expect(await screen.findByText("Saved")).toBeInTheDocument();
		expect(screen.getByText("••••••••")).toBeInTheDocument();
		expect(screen.getByText("Bot token saved, not shown")).toBeInTheDocument();
		expect(document.querySelector("input[type=password]")).toBeNull();

		await user.click(screen.getByRole("button", { name: "Replace bot token" }));
		const field = screen.getByLabelText("Bot token");
		expect(field).toHaveAttribute("type", "password");
		expect(field).toHaveAttribute("autocomplete", "new-password");
		expect(field).toHaveValue("");
		await waitFor(() => expect(field).toHaveFocus());
		await user.type(field, "discarded");
		await user.click(screen.getByRole("button", { name: "Keep saved" }));
		expect(document.querySelector("input[type=password]")).toBeNull();
		expect(screen.getByText("Saved")).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		await waitFor(() => expect(calls.updated).toHaveLength(1));
		expect(calls.updated[0].body.secret).toBeNull();
		expect(document.body.textContent).not.toContain("discarded");

		await router.navigate({
			to: "/settings/notifications/$id",
			params: { id: "id-pager" },
		});
		await user.click(
			await screen.findByRole("button", { name: "Replace bot token" }),
		);
		await user.type(screen.getByLabelText("Bot token"), "fresh-token");
		await user.click(screen.getByRole("button", { name: "Save channel" }));
		await waitFor(() => expect(calls.updated).toHaveLength(2));
		expect(calls.updated[1].body.secret).toBe("fresh-token");
		expect(calls.updated[1].body.settings).toMatchObject({ chat_id: "-100" });
	});

	it("tests a draft channel", async () => {
		const calls = mockChannels([]);
		const user = userEvent.setup();
		await openNotifications(NEW, "Add channel");

		await user.click(screen.getByRole("button", { name: "Send test message" }));
		expect(
			screen.getByText("Enter a name for the channel."),
		).toBeInTheDocument();
		expect(calls.tested).toEqual([]);

		await user.type(screen.getByLabelText("Name"), "hook");
		await user.type(screen.getByLabelText("URL"), "https://am.example.com/h");
		await user.click(screen.getByRole("button", { name: "Send test message" }));
		const status = await screen.findByRole("status");
		expect(status).toHaveTextContent(
			"Test message sent. Check your webhook receiver to confirm it arrived.",
		);
		expect(calls.tested).toHaveLength(1);
		expect(calls.tested[0].channel_id).toBeNull();
		expect(calls.tested[0].channel).toMatchObject({
			type: "webhook",
			name: "hook",
			settings: { url: "https://am.example.com/h" },
		});
		expect(calls.created).toEqual([]);

		server.use(
			http.post("/api/v1/notifications/channels/test", () =>
				HttpResponse.json({ ok: false, message: "connection refused" }),
			),
		);
		await user.click(screen.getByRole("button", { name: "Send test message" }));
		await waitFor(() =>
			expect(screen.getByRole("status")).toHaveTextContent(
				"Couldn't send the test message. Connection refused. Check the channel settings, then try again.",
			),
		);
	});

	it("tests from the row menu", async () => {
		const calls = mockChannels([channel("pager")]);
		const user = userEvent.setup();
		await openNotifications();

		await user.click(
			await screen.findByRole("button", { name: "Actions for pager" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Send test" }),
		);
		expect(
			(await screen.findAllByText("Test message sent to pager.")).length,
		).toBeGreaterThan(0);
		expect(calls.tested).toEqual([{ channel_id: "id-pager" }]);
		await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());

		server.use(
			http.post("/api/v1/notifications/channels/test", () =>
				HttpResponse.json({ ok: false, message: "timeout" }),
			),
		);
		await user.click(screen.getByRole("button", { name: "Actions for pager" }));
		await user.click(
			await screen.findByRole("menuitem", { name: "Send test" }),
		);
		expect(
			(
				await screen.findAllByText(
					"Couldn't send the test message to pager. Timeout. Check the channel settings, then try again.",
				)
			).length,
		).toBeGreaterThan(0);
	});

	it("shows a config channel read only", async () => {
		const calls = mockChannels([
			channel("yaml", {
				type: "telegram",
				source: "config",
				settings: { chat_id: "-100" },
				secret_set: true,
			}),
		]);
		const user = userEvent.setup();
		await openNotifications();

		await user.click(
			await screen.findByRole("button", { name: "Actions for yaml" }),
		);
		expect(await screen.findByRole("menuitem", { name: "View" })).toBeVisible();
		expect(screen.queryByRole("menuitem", { name: "Edit" })).toBeNull();
		expect(
			screen.queryByRole("menuitem", { name: "Delete channel" }),
		).toBeNull();
		await user.click(screen.getByRole("menuitem", { name: "View" }));

		await screen.findByRole("heading", { name: "yaml", level: 1 });
		expect(
			await screen.findByText(
				"This channel is defined in the server configuration. Change it there and restart the server.",
			),
		).toBeInTheDocument();
		expect(screen.getByText("Set in server config")).toBeInTheDocument();
		expect(screen.queryByRole("button", { name: "Save channel" })).toBeNull();
		expect(screen.queryByRole("textbox")).toBeNull();

		await user.click(screen.getByRole("button", { name: "Send test message" }));
		expect(await screen.findByRole("status")).toHaveTextContent(
			"Test message sent. Check chat -100 to confirm it arrived.",
		);
		expect(calls.tested).toEqual([{ channel_id: "id-yaml" }]);
	});
});

describe("notifications: delete and thresholds", () => {
	it("deletes a channel", async () => {
		const calls = mockChannels([channel("pager"), channel("backup-mail")]);
		const user = userEvent.setup();
		await openNotifications();

		let attempts = 0;
		server.use(
			http.delete("/api/v1/notifications/channels/:id", () => {
				attempts += 1;
				if (attempts === 1) {
					return HttpResponse.json(
						{ code: "internal", message: "boom" },
						{ status: 500 },
					);
				}
			}),
		);
		await user.click(
			await screen.findByRole("button", { name: "Actions for pager" }),
		);
		await user.click(
			await screen.findByRole("menuitem", { name: "Delete channel" }),
		);
		const dialog = await screen.findByRole("alertdialog", {
			name: "Delete pager?",
		});
		expect(
			within(dialog).getByText(
				"Alerts stop going to this channel. This cannot be undone.",
			),
		).toBeInTheDocument();
		await waitFor(() =>
			expect(
				within(dialog).getByRole("button", { name: "Keep channel" }),
			).toHaveFocus(),
		);

		await user.click(
			within(dialog).getByRole("button", { name: "Delete channel" }),
		);
		expect(await within(dialog).findByRole("alert")).toHaveTextContent(
			"Couldn't delete the channel. Boom. Try again.",
		);
		expect(calls.deleted).toEqual([]);

		await user.click(
			within(dialog).getByRole("button", { name: "Delete channel" }),
		);
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(calls.deleted).toEqual(["id-pager"]);
		expect(
			(await screen.findAllByText("Channel deleted.")).length,
		).toBeGreaterThan(0);
		await waitFor(() =>
			expect(
				screen.queryByRole("button", { name: "Actions for pager" }),
			).toBeNull(),
		);
		expect(
			screen.getByRole("button", { name: "Actions for backup-mail" }),
		).toBeInTheDocument();
	});

	it("saves thresholds", async () => {
		const calls = mockChannels([]);
		const user = userEvent.setup();
		await openNotifications();

		const offline = await screen.findByLabelText("Agent offline after");
		const cert = screen.getByLabelText("Certificate expires within");
		expect(offline).toHaveValue("15m");
		expect(cert).toHaveValue("14");
		const save = screen.getByRole("button", { name: "Save settings" });
		expect(save).toBeDisabled();

		await user.clear(offline);
		await user.type(offline, "2m");
		await user.clear(cert);
		await user.type(cert, "61");
		await user.click(save);
		expect(
			screen.getByText("Use a duration of 5m or more, such as 15m or 1h."),
		).toBeInTheDocument();
		expect(
			screen.getByText("Enter a whole number of days from 1 to 60."),
		).toBeInTheDocument();
		expect(calls.thresholds).toEqual([]);

		await user.clear(offline);
		await user.type(offline, "30m");
		await user.clear(cert);
		await user.type(cert, "21");
		await user.click(save);
		await waitFor(() =>
			expect(calls.thresholds).toEqual([
				{ offline_after: "30m", cert_days: 21 },
			]),
		);
		expect(
			(
				await screen.findAllByText(
					"Thresholds saved. They apply from the next check.",
				)
			).length,
		).toBeGreaterThan(0);
		await waitFor(() =>
			expect(
				screen.getByRole("button", { name: "Save settings" }),
			).toBeDisabled(),
		);
		expect(screen.getByLabelText("Agent offline after")).toHaveValue("30m");

		await user.clear(screen.getByLabelText("Agent offline after"));
		await user.type(screen.getByLabelText("Agent offline after"), "1h");
		await user.click(screen.getByRole("button", { name: "Discard changes" }));
		expect(screen.getByLabelText("Agent offline after")).toHaveValue("30m");
	});
});

describe("notifications: states", () => {
	it("shows empty, error and not found states", async () => {
		mockChannels([]);
		await openNotifications();
		expect(await screen.findByText("No notification channels")).toBeVisible();
		expect(
			screen.getByText(
				"Alerts show on Overview only. Add Telegram, Slack, email or a webhook to be told when a backup fails.",
			),
		).toBeInTheDocument();
		expect(screen.getAllByRole("button", { name: "Add channel" })).toHaveLength(
			2,
		);
		expect(screen.queryByRole("table")).toBeNull();
		cleanup();

		mockChannels([]);
		server.use(
			http.get("/api/v1/notifications/channels", () => new Promise(() => {})),
		);
		await openNotifications();
		expect(await screen.findAllByTestId("skeleton-row")).toHaveLength(3);
		cleanup();

		mockChannels([]);
		let attempts = 0;
		server.use(
			http.get("/api/v1/notifications/channels", () => {
				attempts += 1;
				return attempts === 1
					? HttpResponse.json(
							{ code: "internal", message: "boom" },
							{ status: 500 },
						)
					: HttpResponse.json([channel("pager")]);
			}),
		);
		const user = userEvent.setup();
		await openNotifications();
		expect(
			await screen.findByText(
				"Couldn't load notification channels. Check that the server is reachable, then retry.",
			),
		).toBeInTheDocument();
		await user.click(screen.getByRole("button", { name: "Retry loading" }));
		expect(await screen.findByText("pager")).toBeInTheDocument();
		cleanup();

		mockChannels([]);
		await openNotifications("/settings/notifications/missing");
		expect(
			await screen.findByText("Channel not found", {
				selector: "[data-slot=empty-title]",
			}),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "Back to notifications" }),
		).toBeInTheDocument();
	});
});

describe("notifications: overview hint and palette", () => {
	it("asks to add a channel when none exist", async () => {
		useSignedIn();
		useFleet(fleetOverview({ healthy: 3 }));
		const { queryClient } = renderApp("/overview");
		expect(
			await screen.findByText("All 3 hosts protected"),
		).toBeInTheDocument();
		const link = await screen.findByRole("link", {
			name: "Set up notifications",
		});
		expect(link).toHaveAttribute("href", "/settings/notifications");
		expect(link.parentElement).toHaveTextContent("Nobody is notified yet.");
		cleanup();
		queryClient.clear();

		mockChannels([channel("pager")]);
		useSignedIn();
		useFleet(fleetOverview({ healthy: 3 }));
		const second = renderApp("/overview");
		expect(
			await screen.findByText("All 3 hosts protected"),
		).toBeInTheDocument();
		await waitFor(() => expect(second.queryClient.isFetching()).toBe(0));
		expect(screen.queryByText(/Nobody is notified yet/)).toBeNull();
		expect(
			screen.queryByRole("link", { name: "Set up notifications" }),
		).toBeNull();
	});

	it("offers notification commands", async () => {
		useSignedIn();
		server.use(http.get("/api/v1/hosts", () => HttpResponse.json([])));
		mockChannels([]);
		const user = userEvent.setup();
		const { router } = renderApp("/overview");
		await screen.findByRole("heading", { level: 1 });

		const openPalette = async () => {
			await user.click(screen.getByRole("button", { name: /^Search…/ }));
			const dialog = await screen.findByRole("dialog", {
				name: "Command palette",
			});
			await user.keyboard("notif");
			return dialog;
		};
		const dialog = await openPalette();
		expect(
			within(dialog)
				.getAllByRole("option")
				.map((o) => o.textContent),
		).toEqual(["Notifications", "Add notification channel"]);
		await user.click(
			screen.getByRole("option", { name: "Add notification channel" }),
		);
		await waitFor(() => expect(router.state.location.pathname).toBe(NEW));

		await openPalette();
		await user.click(screen.getByRole("option", { name: "Notifications" }));
		await waitFor(() =>
			expect(router.state.location.pathname).toBe("/settings/notifications"),
		);
	});
});
