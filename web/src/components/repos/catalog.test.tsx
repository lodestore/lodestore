import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, describe, expect, it } from "vitest";
import type { CatalogStatus } from "@/api/gen/model";
import { apiError, renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const status = (over: Partial<CatalogStatus> = {}): CatalogStatus => ({
	repository_id: "repo-1",
	repository_name: "main",
	phase: "idle",
	read: 0,
	total: null,
	added: 0,
	skipped: [],
	error: null,
	started_at: null,
	finished_at: null,
	last_sync_at: Date.now() - 3 * 3_600_000,
	snapshots: 12,
	...over,
});

function open(statuses: CatalogStatus[]) {
	const current = { statuses };
	useSignedIn();
	server.use(
		http.get("/api/v1/repositories/repo-1", () =>
			HttpResponse.json({
				...repository,
				id: "repo-1",
				location: { backend: "s3", options: { bucket: "b", prefix: "p" } },
				checks: [],
			}),
		),
		http.get("/api/v1/agent-policy", () =>
			HttpResponse.json({ iam_json: "{}", minio_commands: "" }),
		),
		http.get("/api/v1/catalog", () => HttpResponse.json(current.statuses)),
	);
	renderApp("/settings/repositories/repo-1");
	return current;
}

describe("catalog card", () => {
	it("shows the cache explanation and the last sync facts", async () => {
		open([status()]);
		expect(
			await screen.findByRole("heading", { name: "Catalog" }),
		).toBeInTheDocument();
		expect(
			screen.getByText(/The catalog is a cache of the snapshots in storage/),
		).toBeInTheDocument();
		expect(await screen.findByText("12")).toBeInTheDocument();
		expect(screen.getByText("3 hours ago")).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Rebuild catalog" }),
		).toBeEnabled();
	});

	it("shows the phase, a determinate bar and the open-restore link while reading", async () => {
		open([status({ phase: "reading", read: 3, total: 10 })]);
		expect(await screen.findByText("Reading snapshots")).toBeInTheDocument();
		expect(screen.getByRole("progressbar")).toHaveAttribute(
			"aria-valuenow",
			"30",
		);
		expect(screen.getByText("3 of 10 snapshots")).toBeInTheDocument();
		expect(
			screen.getByText(/Snapshots appear in Restore as they are read\./),
		).toBeInTheDocument();
		expect(screen.getByRole("link", { name: "Open Restore" })).toHaveAttribute(
			"href",
			"/restore",
		);
		expect(
			screen.getByRole("button", { name: /Rebuild catalog/ }),
		).toBeDisabled();
	});

	it("keeps the bar indeterminate while listing", async () => {
		open([status({ phase: "listing" })]);
		expect(
			await screen.findByText("Listing snapshots in storage"),
		).toBeInTheDocument();
		expect(screen.getByRole("progressbar")).not.toHaveAttribute(
			"aria-valuenow",
		);
	});

	it("lists skipped snapshots after a rebuild", async () => {
		open([
			status({
				phase: "done",
				skipped: [
					{ key: "snapshots/aa11", reason: "decryption failed" },
					{ key: "snapshots/bb22", reason: "manifest missing" },
				],
			}),
		]);
		expect(
			await screen.findByText(
				/2 snapshots could not be read and were skipped\. Their data is still in storage\./,
			),
		).toBeInTheDocument();
		await userEvent.click(
			screen.getByRole("button", { name: "Skipped snapshots" }),
		);
		expect(
			screen
				.getAllByRole("listitem")
				.find((li) => li.textContent?.startsWith("snapshots/aa11")),
		).toBeTruthy();
		expect(screen.getByText(/manifest missing/)).toBeInTheDocument();
	});

	it("caps the skipped list at ten rows", async () => {
		open([
			status({
				phase: "done",
				skipped: Array.from({ length: 13 }, (_, i) => ({
					key: `snapshots/k${i}`,
					reason: "unreadable",
				})),
			}),
		]);
		await userEvent.click(
			await screen.findByRole("button", { name: "Skipped snapshots" }),
		);
		expect(screen.getByText("and 3 more")).toBeInTheDocument();
		expect(screen.queryByText("snapshots/k10")).not.toBeInTheDocument();
	});

	it("shows a destructive alert when the rebuild failed", async () => {
		open([status({ phase: "failed", error: "access denied" })]);
		expect(
			await screen.findByText(
				"Couldn't rebuild the catalog. Access denied. Check the storage access, then rebuild again.",
			),
		).toBeInTheDocument();
		expect(
			screen.getByRole("button", { name: "Rebuild catalog" }),
		).toBeEnabled();
	});

	it("starts a rebuild and shows the spinner", async () => {
		const current = open([status()]);
		let posts = 0;
		let release = () => {};
		const gate = new Promise<void>((resolve) => {
			release = resolve;
		});
		server.use(
			http.post("/api/v1/repositories/repo-1/catalog/rebuild", async () => {
				posts += 1;
				await gate;
				current.statuses = [status({ phase: "listing" })];
				return HttpResponse.json(current.statuses[0], { status: 202 });
			}),
		);
		const button = await screen.findByRole("button", {
			name: "Rebuild catalog",
		});
		await userEvent.click(button);
		await waitFor(() => expect(button).toBeDisabled());
		expect(button.querySelector("[data-slot=spinner]")).not.toBeNull();
		expect(posts).toBe(1);
		release();
		expect(
			await screen.findByText("Listing snapshots in storage"),
		).toBeInTheDocument();
	});

	it("reports a refused rebuild", async () => {
		open([status()]);
		server.use(
			http.post("/api/v1/repositories/repo-1/catalog/rebuild", () =>
				apiError(502, "storage", "bucket unreachable"),
			),
		);
		await userEvent.click(
			await screen.findByRole("button", { name: "Rebuild catalog" }),
		);
		expect(
			await screen.findByText(
				/Couldn't rebuild the catalog\. Bucket unreachable\./,
			),
		).toBeInTheDocument();
	});
});

describe("restore timeline during a rebuild", () => {
	function openTimeline(phase: CatalogStatus["phase"]) {
		useSignedIn();
		window.innerWidth = 1440;
		server.use(
			http.get("/api/v1/catalog", () => HttpResponse.json([status({ phase })])),
			http.get("/api/v1/restore/hosts", () =>
				HttpResponse.json([
					{
						key: "agent-web",
						host_id: "agent-web",
						hostname: "web-1",
						agent: true,
						status: "online",
						labels: [],
						repository_id: "repo-1",
						repository_name: "main",
						snapshot_count: 1,
						latest_snapshot_ms: Date.now(),
						unverified_count: 0,
					},
				]),
			),
			http.get("/api/v1/restore/snapshots", () =>
				HttpResponse.json({
					items: [
						{
							id: "a".repeat(64),
							hostname: "web-1",
							host_key: "agent-web",
							repository_id: "repo-1",
							time: Date.now(),
							incomplete: false,
							paths: ["/etc"],
							tags: [],
							warnings_total: 0,
							errors_total: 0,
							stats: {
								bytes_total: 1024,
								bytes_added: 1024,
								bytes_read: 1024,
								dirs: 1,
								files_new: 1,
								files_changed: 0,
								files_unchanged: 0,
							},
						},
					],
					total: 1,
					next: null,
				}),
			),
			http.get(
				"/api/v1/snapshots/:id",
				() => new HttpResponse(null, { status: 404 }),
			),
		);
		renderApp("/restore");
	}

	afterEach(() => {
		window.innerWidth = 1024;
	});

	it("says more snapshots may appear while the host's repository rebuilds", async () => {
		openTimeline("reading");
		expect(
			await screen.findByText("More snapshots may still appear."),
		).toBeInTheDocument();
	});

	it("stays quiet when nothing is rebuilding", async () => {
		openTimeline("idle");
		await screen.findByText("Showing 1 of 1 snapshots");
		expect(
			screen.queryByText("More snapshots may still appear."),
		).not.toBeInTheDocument();
	});
});
