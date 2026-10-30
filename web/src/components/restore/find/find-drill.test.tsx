import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { renderApp, useSignedIn } from "@/test/app";
import { findGroup, findHandlers, findSession } from "@/test/find";

const realMatchMedia = window.matchMedia;

function setWide(wide: boolean) {
	window.matchMedia = ((query: string) => ({
		matches: wide && query.includes("min-width: 1024px"),
		media: query,
		addEventListener: () => {},
		removeEventListener: () => {},
	})) as unknown as typeof window.matchMedia;
}

async function open(wide: boolean, path = "/restore?tab=find&find=*.conf") {
	setWide(wide);
	useSignedIn();
	findHandlers([
		findSession({
			state: "done",
			groups: [findGroup(0), findGroup(1, { hostname: "db-1" })],
		}),
	]);
	const view = renderApp(path);
	const list = within(
		await screen.findByRole("navigation", { name: "Matching files" }),
	);
	await waitFor(() => expect(list.getAllByRole("link")).toHaveLength(2));
	return { ...view, list };
}

const listPane = () =>
	screen.getByRole("navigation", { name: "Matching files" }).parentElement;
const detailPane = () =>
	screen.getByRole("button", { name: "Back to results" }).parentElement;

describe("Find file drill-in", { timeout: 30_000 }, () => {
	afterEach(() => {
		window.matchMedia = realMatchMedia;
	});

	it("shows only the list on a narrow screen until a file is picked", async () => {
		await open(false);
		expect(listPane()).not.toHaveClass("hidden");
		expect(detailPane()).toHaveClass("hidden", "lg:flex");
	});

	it("drills into the details with focus on the heading and back to the list item", async () => {
		const { router, list } = await open(false);
		const user = userEvent.setup();
		await user.click(list.getAllByRole("link")[1]);

		await waitFor(() =>
			expect(router.state.location.search).toMatchObject({
				file: `agent-0/${findGroup(0).path_b64}`,
			}),
		);
		expect(listPane()).toHaveClass("hidden", "lg:flex");
		expect(detailPane()).not.toHaveClass("hidden");
		await waitFor(() =>
			expect(screen.getByRole("heading", { level: 2 })).toHaveFocus(),
		);

		await user.click(screen.getByRole("button", { name: "Back to results" }));
		await waitFor(() =>
			expect(router.state.location.search.file).toBeUndefined(),
		);
		expect(listPane()).not.toHaveClass("hidden");
		await waitFor(() => expect(list.getAllByRole("link")[1]).toHaveFocus());
	});

	it("leaves a deep-linked file through Back to results without going back", async () => {
		const { router } = await open(
			false,
			`/restore?tab=find&find=*.conf&file=agent-1/${findGroup(1).path_b64}`,
		);
		const user = userEvent.setup();
		await user.click(screen.getByRole("button", { name: "Back to results" }));
		await waitFor(() =>
			expect(router.state.location.search.file).toBeUndefined(),
		);
		expect(router.state.location.search).toMatchObject({ find: "*.conf" });
	});

	it("shows a placeholder, not another file, until the linked file arrives", async () => {
		setWide(false);
		useSignedIn();
		findHandlers([
			findSession({
				revision: 1,
				groups: [findGroup(1, { hostname: "db-1" })],
			}),
			findSession({
				revision: 2,
				state: "done",
				groups: [findGroup(0)],
			}),
		]);
		renderApp(
			`/restore?tab=find&find=*.conf&file=agent-0/${findGroup(0).path_b64}`,
		);
		const back = await screen.findByRole("button", { name: "Back to results" });
		expect(screen.queryByRole("heading", { level: 2 })).not.toBeInTheDocument();
		expect(back.parentElement?.querySelector("[aria-hidden]")).not.toBeNull();
		expect(
			screen
				.getByRole("navigation", { name: "Matching files" })
				.querySelector("[aria-current]"),
		).toBeNull();

		const heading = await screen.findByRole(
			"heading",
			{ level: 2 },
			{ timeout: 5000 },
		);
		expect(heading).toHaveTextContent("nginx.conf");
		const list = within(
			screen.getByRole("navigation", { name: "Matching files" }),
		);
		const [first, second] = list.getAllByRole("link");
		expect(first).toHaveTextContent("db-1");
		expect(second).toHaveAttribute("aria-current", "true");
	});

	it("falls back to the top result when the search ends without the linked file", async () => {
		await open(true, "/restore?tab=find&find=*.conf&file=agent-9/AAAA");
		const list = within(
			screen.getByRole("navigation", { name: "Matching files" }),
		);
		expect(list.getAllByRole("link")[0]).toHaveAttribute(
			"aria-current",
			"true",
		);
	});

	it("keeps focus in the list on a wide screen", async () => {
		const { list } = await open(true);
		const user = userEvent.setup();
		await user.click(list.getAllByRole("link")[1]);

		await waitFor(() =>
			expect(list.getAllByRole("link")[1]).toHaveAttribute(
				"aria-current",
				"true",
			),
		);
		expect(list.getAllByRole("link")[1]).toHaveFocus();
		expect(screen.getByRole("heading", { level: 2 })).not.toHaveFocus();
		expect(screen.getByRole("button", { name: "Back to results" })).toHaveClass(
			"lg:hidden",
		);
	});
});
