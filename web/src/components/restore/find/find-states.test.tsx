import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { RestoreHost } from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import { DAY, findGroup, findHandlers, findSession } from "@/test/find";
import { server } from "@/test/server";

function restoreHost(n: number, labels: RestoreHost["labels"] = []) {
	return {
		key: `agent-${n}`,
		host_id: `agent-${n}`,
		hostname: `web-${n}`,
		agent: true,
		status: "online",
		labels,
		repository_id: "repo-1",
		repository_name: "main",
		snapshot_count: 3,
		latest_snapshot_ms: Date.UTC(2026, 9, 21),
		unverified_count: 0,
		restore_roots: [],
	} satisfies RestoreHost;
}

function mockHosts(hosts: RestoreHost[]) {
	server.use(http.get("/api/v1/restore/hosts", () => HttpResponse.json(hosts)));
}

const FIELD = { name: "File name or pattern" };
const doneSession = findSession({ state: "done", scanned: 4 });

describe("Find file filters", { timeout: 30_000 }, () => {
	it("narrows the search to the chosen hosts", async () => {
		useSignedIn();
		mockHosts([restoreHost(0), restoreHost(1), restoreHost(2)]);
		const { calls } = findHandlers([doneSession]);
		const user = userEvent.setup();
		renderApp("/restore?tab=find&find=*.conf");

		await user.click(await screen.findByRole("button", { name: "All hosts" }));
		await user.click(await screen.findByRole("option", { name: /web-0/ }));
		await user.click(screen.getByRole("option", { name: /web-1/ }));
		await user.keyboard("{Escape}");

		expect(
			await screen.findByRole("button", { name: "2 hosts" }),
		).toBeVisible();
		await waitFor(() =>
			expect(calls.bodies.at(-1)).toEqual({
				pattern: "*.conf",
				hosts: ["agent-0", "agent-1"],
			}),
		);
	});

	it("narrows the search by label", async () => {
		useSignedIn();
		mockHosts([restoreHost(0, [{ key: "role", value: "web" }])]);
		const { calls } = findHandlers([doneSession]);
		const user = userEvent.setup();
		renderApp("/restore?tab=find&find=*.conf");

		await user.click(await screen.findByRole("button", { name: "All hosts" }));
		await user.click(
			within(await screen.findByRole("group", { name: "Labels" })).getByRole(
				"option",
				{ name: /role=web/ },
			),
		);
		await user.keyboard("{Escape}");

		expect(
			await screen.findByRole("button", { name: "role=web" }),
		).toBeVisible();
		await waitFor(() =>
			expect(calls.bodies.at(-1)).toEqual({
				pattern: "*.conf",
				labels: ["role=web"],
			}),
		);
	});

	it("limits the period to the last 7 days", async () => {
		useSignedIn();
		mockHosts([restoreHost(0)]);
		const { calls } = findHandlers([doneSession]);
		const user = userEvent.setup();
		renderApp("/restore?tab=find&find=*.conf");

		await user.click(await screen.findByRole("button", { name: "Any time" }));
		await user.click(await screen.findByRole("radio", { name: "Last 7 days" }));

		expect(
			await screen.findByRole("button", { name: "Last 7 days" }),
		).toBeVisible();
		await waitFor(() => expect(calls.bodies).toHaveLength(2));
		const body = calls.bodies.at(-1) as { since: number; until?: number };
		expect(Math.abs(body.since - (Date.now() - 7 * DAY))).toBeLessThan(DAY);
		expect(body.until).toBeUndefined();
	});

	it("takes a custom range", async () => {
		useSignedIn();
		mockHosts([restoreHost(0)]);
		const { calls } = findHandlers([doneSession]);
		const user = userEvent.setup();
		renderApp("/restore?tab=find&find=*.conf");

		await user.click(await screen.findByRole("button", { name: "Any time" }));
		await user.click(
			await screen.findByRole("radio", { name: "Custom range" }),
		);
		fireEvent.change(screen.getByLabelText("From"), {
			target: { value: "2026-10-01" },
		});
		fireEvent.change(screen.getByLabelText("To"), {
			target: { value: "2026-10-05" },
		});

		await waitFor(() =>
			expect(calls.bodies.at(-1)).toEqual({
				pattern: "*.conf",
				since: new Date(2026, 9, 1).getTime(),
				until: new Date(2026, 9, 6).getTime() - 1,
			}),
		);
	});

	it("restores the filters from the address", async () => {
		useSignedIn();
		mockHosts([restoreHost(0), restoreHost(1)]);
		const { calls } = findHandlers([doneSession]);
		renderApp(
			"/restore?tab=find&find=*.conf&hosts=agent-0,agent-1&labels=env=prod",
		);

		expect(
			await screen.findByRole("button", { name: "2 hosts, env=prod" }),
		).toBeVisible();
		await waitFor(() =>
			expect(calls.bodies).toEqual([
				{
					pattern: "*.conf",
					hosts: ["agent-0", "agent-1"],
					labels: ["env=prod"],
				},
			]),
		);
		expect(
			within(screen.getByRole("search").parentElement as HTMLElement).getByText(
				/Searches every snapshot/,
			),
		).toBeVisible();
	});
});

const line = (text: string | RegExp) =>
	screen.findByText(text, { selector: "p" });
const live = () =>
	document.querySelector<HTMLElement>('[aria-live="polite"]') as HTMLElement;

describe("Find file session states", { timeout: 30_000 }, () => {
	function open(path: string, hosts = [restoreHost(0)]) {
		useSignedIn();
		mockHosts(hosts);
		return renderApp(path);
	}

	it("shows progress while searching and stops on request", async () => {
		const { calls } = findHandlers([
			findSession({ scanned: 3, total: 10, hosts: 2, files: 1, versions: 1 }),
		]);
		const user = userEvent.setup();
		open("/restore?tab=find&find=*.conf");

		expect(
			await line(
				"Searching · 3 of 10 snapshots on 2 hosts · 1 file, 1 version so far",
			),
		).toBeVisible();
		expect(await screen.findByRole("progressbar")).toHaveAttribute(
			"aria-valuetext",
			"3 of 10 snapshots",
		);
		await user.click(screen.getByRole("button", { name: "Stop" }));
		await waitFor(() => expect(calls.cancel).toBe(1));
	});

	it("shows skeleton items while nothing has arrived", async () => {
		findHandlers([findSession()]);
		open("/restore?tab=find&find=*.conf");
		await screen.findByRole("progressbar");
		expect(document.querySelectorAll('[data-slot="skeleton"]')).toHaveLength(3);
	});

	it("announces the final state at once", async () => {
		findHandlers([
			findSession({
				state: "done",
				scanned: 4,
				total: 4,
				hosts: 2,
				files: 1,
				versions: 1,
				finished_at: Date.UTC(2026, 9, 29, 12, 1, 15),
				groups: [findGroup(0)],
			}),
		]);
		open("/restore?tab=find&find=*.conf");

		const text =
			"Done · searched 4 snapshots on 2 hosts in 1 min 15 s · 1 file, 1 version";
		expect(await line(text)).toBeVisible();
		await waitFor(() => expect(live()).toHaveTextContent(text));
		expect(screen.queryByRole("progressbar")).toBeNull();
	});

	it("keeps the results when the match limit is reached", async () => {
		findHandlers([
			findSession({
				state: "incomplete",
				stop_reason: "limit",
				limit: 1000,
				groups: [findGroup(0)],
			}),
		]);
		open("/restore?tab=find&find=*.conf");

		expect(
			await screen.findByText(
				"Stopped at 1000 matches. Showing what was found. Narrow the pattern or add filters.",
			),
		).toBeVisible();
		expect(
			within(
				await screen.findByRole("navigation", { name: "Matching files" }),
			).getAllByRole("link"),
		).toHaveLength(1);
	});

	it("says when the time limit stopped the search", async () => {
		findHandlers([
			findSession({
				state: "incomplete",
				stop_reason: "timeout",
				finished_at: Date.UTC(2026, 9, 29, 12, 0, 30),
			}),
		]);
		open("/restore?tab=find&find=*.conf");

		expect(
			await screen.findByText(
				"Stopped after 30 s. Showing what was found. Narrow the pattern or add filters.",
			),
		).toBeVisible();
	});

	it("says when the search was stopped", async () => {
		findHandlers([findSession({ state: "cancelled" })]);
		open("/restore?tab=find&find=*.conf");
		expect(await line("Search stopped. Showing what was found.")).toBeVisible();
	});

	it("offers a retry when the search failed", async () => {
		const { calls } = findHandlers([
			findSession({ state: "failed", error: "storage offline" }),
			findSession({ state: "done" }),
		]);
		const user = userEvent.setup();
		open("/restore?tab=find&find=*.conf");

		expect(
			await screen.findByText("Couldn't finish the search: storage offline"),
		).toBeVisible();
		await user.click(screen.getByRole("button", { name: /Retry/ }));
		await waitFor(() => expect(calls.start).toBe(2));
	});

	it("explains an empty result", async () => {
		findHandlers([findSession({ state: "done" })]);
		open("/restore?tab=find&find=x.y");

		expect(await screen.findByText("No files match “x.y”.")).toBeVisible();
		expect(
			screen.getByText(
				"Check the spelling, use * for any part of a name, or widen the period.",
			),
		).toBeVisible();
	});

	it("shows a rejected pattern as a field error without a retry button", async () => {
		server.use(
			http.post("/api/v1/find", () =>
				HttpResponse.json(
					{
						errors: [
							{
								path: "pattern",
								message: "The pattern is not valid: unclosed character class.",
							},
						],
					},
					{ status: 422 },
				),
			),
		);
		open("/restore?tab=find&find=[abc");

		const field = await screen.findByRole("searchbox", FIELD);
		await waitFor(() => expect(field).toHaveAttribute("aria-invalid", "true"));
		expect(field).toHaveAccessibleDescription(
			"The pattern is not valid: unclosed character class.",
		);
		expect(
			screen.queryByRole("button", { name: "Retry loading" }),
		).not.toBeInTheDocument();
	});

	it("keeps the retry alert for a rejected filter", async () => {
		server.use(
			http.post("/api/v1/find", () =>
				HttpResponse.json(
					{
						code: "validation",
						message: "Unknown host.",
						errors: [{ path: "hosts", message: "Unknown host." }],
					},
					{ status: 422 },
				),
			),
		);
		open("/restore?tab=find&find=*.conf&hosts=gone");

		expect(
			await screen.findByRole("button", { name: "Retry loading" }),
		).toBeInTheDocument();
		expect(screen.getByRole("searchbox", FIELD)).not.toHaveAttribute(
			"aria-invalid",
		);
	});

	it("keeps the retry alert for other failures to start", async () => {
		server.use(
			http.post("/api/v1/find", () =>
				HttpResponse.json(
					{ code: "internal", message: "boom" },
					{ status: 500 },
				),
			),
		);
		open("/restore?tab=find&find=*.conf");

		expect(
			await screen.findByRole("button", { name: "Retry loading" }),
		).toBeVisible();
		expect(screen.getByRole("searchbox", FIELD)).not.toHaveAttribute(
			"aria-invalid",
			"true",
		);
	});

	it("says no snapshots match when filters leave nothing to search", async () => {
		findHandlers([findSession({ state: "done", total: 0 })]);
		open("/restore?tab=find&find=x.y&hosts=agent-0");

		expect(
			await screen.findByText("No snapshots match these filters."),
		).toBeVisible();
		expect(
			screen.getByText("Choose other hosts or labels, or widen the period."),
		).toBeVisible();
		expect(screen.queryByText(/Check the spelling/)).not.toBeInTheDocument();
	});

	it("says how many snapshots could not be read", async () => {
		findHandlers([findSession({ state: "done", skipped: 2 })]);
		open("/restore?tab=find&find=x.y");

		expect(
			await screen.findByText(
				/widen the period\. 2 snapshots couldn't be read\./,
			),
		).toBeVisible();
		expect(
			screen.getByText(/Done · .*2 snapshots couldn't be read/, {
				selector: "p",
			}),
		).toBeVisible();
	});

	it("suggests examples before the first search", async () => {
		const { calls } = findHandlers([findSession({ state: "done" })]);
		const user = userEvent.setup();
		const { router } = open("/restore?tab=find");

		expect(await screen.findByText("Find a file on any host")).toBeVisible();
		for (const example of ["nginx.conf", "*.sql.gz", "/etc/nginx/**"]) {
			expect(screen.getByRole("button", { name: example })).toBeVisible();
		}
		await user.click(screen.getByRole("button", { name: "*.sql.gz" }));

		expect(screen.getByRole("searchbox", FIELD)).toHaveValue("*.sql.gz");
		await waitFor(() =>
			expect(calls.bodies).toEqual([{ pattern: "*.sql.gz" }]),
		);
		expect(router.state.location.search).toMatchObject({ find: "*.sql.gz" });
	});

	it("points to the setup when there are no snapshots at all", async () => {
		open("/restore?tab=find", []);
		expect(await screen.findByText("Nothing to restore yet")).toBeVisible();
		expect(screen.queryByText("Find a file on any host")).toBeNull();
	});
});
