import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
	act,
	renderHook,
	screen,
	waitFor,
	within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import type { ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";
import type {
	FindSession,
	RestoreHost,
	RestoreSnapshot,
} from "@/api/gen/model";
import { renderApp, useSignedIn } from "@/test/app";
import {
	findGroup,
	findHandlers,
	findId,
	findSession,
	findSnapshotRef,
	findVersion,
} from "@/test/find";
import { server } from "@/test/server";
import { findRequest, useFindSession } from "./use-find-session";

const local = (day: number) => new Date(2026, 9, day, 12).getTime();
const FIELD = { name: "File name or pattern" };

async function results() {
	return within(
		await screen.findByRole("navigation", { name: "Matching files" }),
	);
}

describe("Find file tab", { timeout: 30_000 }, () => {
	it("opens on the find tab with the search field focused", async () => {
		useSignedIn();
		renderApp("/restore?tab=find");
		const field = await screen.findByRole("searchbox", FIELD);
		expect(field).toHaveFocus();
		expect(screen.getByRole("tab", { name: "Find file" })).toHaveAttribute(
			"aria-selected",
			"true",
		);
		expect(screen.getByRole("button", { name: "Search" })).toBeDisabled();
	});

	it("sorts files by host while the search runs", async () => {
		useSignedIn();
		const { calls, afters } = findHandlers([
			findSession({ revision: 1, groups: [findGroup(0)] }),
			findSession({
				revision: 2,
				state: "done",
				groups: [findGroup(1, { hostname: "db-1", path: "/etc/my.conf" })],
			}),
		]);
		renderApp("/restore?tab=find");
		const user = userEvent.setup();
		await user.type(
			await screen.findByRole("searchbox", FIELD),
			"*.conf{Enter}",
		);

		const list = await results();
		await waitFor(() => expect(list.getAllByRole("link")).toHaveLength(2));
		expect(calls.bodies).toEqual([{ pattern: "*.conf" }]);
		expect(afters.slice(0, 2)).toEqual(["0", "1"]);
		const [first, second] = list.getAllByRole("link");
		expect(first).toHaveTextContent("db-1");
		expect(second).toHaveTextContent("web-0");
		expect(first).toHaveAttribute("aria-current", "true");
	});

	it("lists hosts in a fixed order whatever order they arrive in", async () => {
		useSignedIn();
		findHandlers([
			findSession({
				state: "done",
				groups: [
					findGroup(0, { hostname: "web-2", host_key: "agent-2" }),
					findGroup(1, { hostname: "web-0", host_key: "agent-0" }),
				],
			}),
		]);
		renderApp("/restore?tab=find&find=*.conf");
		const list = await results();
		await waitFor(() => expect(list.getAllByRole("link")).toHaveLength(2));
		const [first, second] = list.getAllByRole("link");
		expect(first).toHaveTextContent("web-0");
		expect(second).toHaveTextContent("web-2");
		expect(first).toHaveAttribute("aria-current", "true");
	});

	it("selects the group named by the file key whatever its arrival index", async () => {
		useSignedIn();
		findHandlers([
			findSession({
				state: "done",
				groups: [
					findGroup(0, { hostname: "web-2", host_key: "agent-2" }),
					findGroup(1, { hostname: "web-0", host_key: "agent-0" }),
				],
			}),
		]);
		renderApp(
			`/restore?tab=find&find=*.conf&file=agent-2/${findGroup(0).path_b64}`,
		);
		const list = await results();
		await waitFor(() => expect(list.getAllByRole("link")).toHaveLength(2));
		const [first, second] = list.getAllByRole("link");
		expect(second).toHaveAttribute("aria-current", "true");
		expect(first).not.toHaveAttribute("aria-current");
	});

	it("selects a result through the url", async () => {
		useSignedIn();
		findHandlers([
			findSession({
				state: "done",
				groups: [findGroup(0), findGroup(1, { hostname: "db-1" })],
			}),
		]);
		const { router } = renderApp("/restore?tab=find&find=*.conf");
		const user = userEvent.setup();
		const list = await results();
		await waitFor(() => expect(list.getAllByRole("link")).toHaveLength(2));
		await user.click(list.getAllByRole("link")[1]);

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				file: `agent-0/${findGroup(0).path_b64}`,
			}),
		);
		const [first, second] = list.getAllByRole("link");
		expect(second).toHaveAttribute("aria-current", "true");
		expect(first).not.toHaveAttribute("aria-current");
	});

	describe("details", () => {
		const newest = findVersion({
			first_ms: local(15),
			last_ms: local(21),
			snapshot_count: 14,
			snapshots: [findSnapshotRef(9), findSnapshotRef(8)],
		});
		const older = findVersion({
			content_id: "8f2b00000000000000000000000000013dc",
			first_ms: local(1),
			last_ms: local(14),
			snapshot_count: 28,
			snapshots: [findSnapshotRef(3)],
		});
		const snapshot: RestoreSnapshot = {
			id: findId(9),
			hostname: "web-0",
			host_key: "agent-0",
			repository_id: "repo-1",
			time: Date.UTC(2026, 9, 21, 2, 0),
			incomplete: false,
			paths: ["/etc"],
			policy_name: "etc",
			policy_deleted: false,
			received_at: Date.UTC(2026, 9, 21, 2, 1),
			verified_by: "server",
			tags: [],
			warnings_total: 0,
			errors_total: 0,
			stats: {
				bytes_total: 2_097_152,
				bytes_added: 1024,
				bytes_read: 2_097_152,
				dirs: 1,
				files_new: 2,
				files_changed: 0,
				files_unchanged: 0,
			},
		};
		const host: RestoreHost = {
			key: "agent-0",
			host_id: "agent-0",
			hostname: "web-0",
			agent: true,
			status: "online",
			labels: [],
			repository_id: "repo-1",
			repository_name: "main",
			snapshot_count: 3,
			latest_snapshot_ms: snapshot.time,
			unverified_count: 0,
		};

		function open() {
			useSignedIn();
			findHandlers([
				findSession({
					state: "done",
					groups: [findGroup(0, { versions: [older, newest] })],
				}),
			]);
			return renderApp("/restore?tab=find&find=nginx.conf");
		}

		it("lists versions newest first under the file path", async () => {
			open();
			const heading = await screen.findByRole("heading", {
				level: 2,
				name: "/etc/nginx/nginx.conf",
			});
			expect(heading).toHaveAttribute("tabindex", "-1");

			const buttons = screen.getAllByRole("button", { name: /^Restore / });
			expect(buttons).toHaveLength(2);
			expect(buttons[0]).toHaveAccessibleName(
				/^Restore \/etc\/nginx\/nginx\.conf from .*web-0.*, version Oct 15 – Oct 21$/u,
			);
			expect(buttons[1]).toHaveAccessibleName(/version Oct 1 – Oct 14$/u);
			expect(screen.getByText("Present in 14 snapshots")).toBeVisible();
			expect(screen.getByText("c41e…9a07")).toHaveAttribute(
				"title",
				newest.content_id,
			);
		});

		it("restores the newest version through the Phase 4 dialog", async () => {
			let summaryBody: unknown;
			server.use(
				http.get("/api/v1/restore/hosts", () => HttpResponse.json([host])),
				http.get("/api/v1/snapshots/:id", ({ params }) => {
					expect(params.id).toBe(findId(9));
					return HttpResponse.json(snapshot);
				}),
				http.post("/api/v1/snapshots/:id/summary", async ({ request }) => {
					summaryBody = await request.json();
					return HttpResponse.json({ files: 1, dirs: 0, bytes: 3174 });
				}),
			);
			open();
			const user = userEvent.setup();
			const [restore] = await screen.findAllByRole("button", {
				name: /^Restore /,
			});
			await user.click(restore);

			const dialog = await screen.findByRole("dialog");
			expect(
				await within(dialog).findByRole("heading", { name: "Restore 1 item" }),
			).toBeVisible();
			await waitFor(() =>
				expect(summaryBody).toEqual({
					items: [findGroup(0).path_b64],
				}),
			);
		});

		it("keeps the older version restore as an outline button", async () => {
			open();
			const buttons = await screen.findAllByRole("button", {
				name: /^Restore /,
			});
			expect(buttons[0]).toHaveAttribute("data-variant", "default");
			expect(buttons[1]).toHaveAttribute("data-variant", "outline");
		});
	});

	describe("session lifecycle", () => {
		const settle = (ms: number) =>
			new Promise((resolve) => setTimeout(resolve, ms));

		function trace(sessions: Record<string, FindSession>) {
			const events: string[] = [];
			let started = 0;
			server.use(
				http.post("/api/v1/find", async ({ request }) => {
					started += 1;
					const body = (await request.json()) as { pattern: string };
					events.push(`start ${body.pattern}`);
					return HttpResponse.json({ id: `find-${started}` });
				}),
				http.get("/api/v1/find/:id", ({ params }) => {
					events.push(`get ${params.id}`);
					return HttpResponse.json(sessions[params.id as string]);
				}),
				http.delete("/api/v1/find/:id", ({ params }) => {
					events.push(`delete ${params.id}`);
					return new HttpResponse(null, { status: 204 });
				}),
			);
			return events;
		}

		it("cancels the live session before a new search starts", async () => {
			useSignedIn();
			const events = trace({
				"find-1": findSession({ id: "find-1", groups: [findGroup(0)] }),
				"find-2": findSession({ id: "find-2", state: "done" }),
			});
			renderApp("/restore?tab=find&find=*.conf");
			const user = userEvent.setup();
			await results();

			const field = screen.getByRole("searchbox", FIELD);
			await user.clear(field);
			await user.type(field, "*.sql{Enter}");

			await waitFor(() => expect(events).toContain("get find-2"));
			const order = events.filter((event) => !event.startsWith("get"));
			expect(order).toEqual(["start *.conf", "delete find-1", "start *.sql"]);
		});

		it("does not search again when the tab is left and reopened", async () => {
			useSignedIn();
			server.use(
				http.get("/api/v1/restore/hosts", () => HttpResponse.json([])),
			);
			const { calls } = findHandlers([
				findSession({ state: "done", groups: [findGroup(0)] }),
			]);
			renderApp("/restore?tab=find&find=*.conf");
			const user = userEvent.setup();
			await results();

			await user.click(screen.getByRole("tab", { name: "By host" }));
			await screen.findByText("Nothing to restore yet");
			await user.click(screen.getByRole("tab", { name: "Find file" }));

			expect(await results()).toBeTruthy();
			expect(calls.start).toBe(1);
			expect(calls.get).toBe(1);
			expect(calls.cancel).toBe(0);
		});

		it("starts a new search when Search is pressed again with the same pattern", async () => {
			useSignedIn();
			const { calls } = findHandlers([
				findSession({ state: "done", groups: [findGroup(0)] }),
			]);
			renderApp("/restore?tab=find&find=*.conf");
			const user = userEvent.setup();
			await results();

			await user.click(screen.getByRole("button", { name: "Search" }));
			await waitFor(() => expect(calls.start).toBe(2));
		});

		it("starts a search again when going back to one another search cancelled", async () => {
			let first = findSession({ id: "find-1" });
			const events: string[] = [];
			let started = 0;
			server.use(
				http.post("/api/v1/find", () => {
					started += 1;
					events.push(`start ${started}`);
					return HttpResponse.json({ id: `find-${started}` });
				}),
				http.get("/api/v1/find/:id", ({ params }) =>
					HttpResponse.json(
						params.id === "find-1"
							? first
							: findSession({ id: params.id as string, state: "done" }),
					),
				),
				http.delete(
					"/api/v1/find/:id",
					() => new HttpResponse(null, { status: 204 }),
				),
			);
			const wrapper = ({ children }: { children: ReactNode }) => (
				<QueryClientProvider client={new QueryClient()}>
					{children}
				</QueryClientProvider>
			);
			const { result, rerender } = renderHook(
				({ pattern, run }) => useFindSession(findRequest(pattern), run),
				{ wrapper, initialProps: { pattern: "a", run: 1 } },
			);
			await waitFor(() => expect(result.current.session?.id).toBe("find-1"));
			rerender({ pattern: "b", run: 2 });
			await waitFor(() => expect(result.current.session?.id).toBe("find-2"));
			first = findSession({ id: "find-1", state: "cancelled" });
			rerender({ pattern: "a", run: 1 });

			await waitFor(() => expect(result.current.session?.id).toBe("find-3"), {
				timeout: 5000,
			});
			expect(events).toEqual(["start 1", "start 2", "start 3"]);
		});

		it("starts straight away when the page loads with a pattern", async () => {
			useSignedIn();
			const { calls } = findHandlers([findSession({ state: "done" })]);
			renderApp("/restore?tab=find&find=nginx.conf");
			await waitFor(() => expect(calls.start).toBe(1));
			expect(calls.bodies).toEqual([{ pattern: "nginx.conf" }]);
		});

		it("stops polling once the search is done", async () => {
			vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
			try {
				useSignedIn();
				const { calls, afters } = findHandlers(
					[findSession({ state: "done", groups: [findGroup(0)] })],
					"find-stops-polling",
				);
				renderApp("/restore?tab=find&find=*.conf");
				await results();
				await vi.advanceTimersByTimeAsync(2200);
				expect(calls.get, `after=${afters}`).toBe(1);
			} finally {
				vi.useRealTimers();
			}
		});

		it("stop() cancels the session and polling ends", async () => {
			const { calls } = findHandlers([
				findSession({ revision: 1, groups: [findGroup(0)] }),
				findSession({ revision: 2, state: "cancelled" }),
			]);
			const wrapper = ({ children }: { children: ReactNode }) => (
				<QueryClientProvider client={new QueryClient()}>
					{children}
				</QueryClientProvider>
			);
			const { result } = renderHook(
				() => useFindSession(findRequest("*.conf")),
				{ wrapper },
			);
			await waitFor(() => expect(result.current.groups).toHaveLength(1));

			await act(() => result.current.stop());
			await waitFor(() =>
				expect(result.current.session?.state).toBe("cancelled"),
			);
			expect(calls.cancel).toBe(1);
			const seen = calls.get;
			await settle(1500);
			expect(calls.get).toBe(seen);
			expect(result.current.groups).toHaveLength(1);
		});
	});
});
