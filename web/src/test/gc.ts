import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type { GcCycleDto, GcReport, GcStatus } from "@/api/gen/model";
import { renderApp, repository, useSignedIn } from "./app";
import { server } from "./server";

export const GIB = 1024 ** 3;

export const gcReport = (over: Partial<GcReport> = {}): GcReport => ({
	cycle_id: "cycle-1",
	dry_run: true,
	finished_at: Date.now() - 2 * 3_600_000,
	duration_ms: 95_000,
	bytes_freed: 3 * GIB,
	packs_deleted: 318,
	packs_repacked: 0,
	repack_transfer_bytes: 0,
	locked_bytes: 0,
	locked_until: null,
	locked_until_unknown: false,
	...over,
});

export const gcCycle = (over: Partial<GcCycleDto> = {}): GcCycleDto => ({
	id: "cycle-2",
	state: "marking",
	dry_run: false,
	origin: "manual",
	started_at: Date.now() - 60_000,
	delete_after: null,
	progress: null,
	marked_packs: 0,
	marked_bytes: 0,
	blocked_by_writers: [],
	waiting_for_restores: false,
	error: null,
	error_after_delete: false,
	...over,
});

export const gcStatus = (over: Partial<GcStatus> = {}): GcStatus => ({
	repository_id: "repo-1",
	enabled: false,
	settings: {
		cron: "0 3 * * 0",
		timezone: "UTC",
		pause: "24h",
		repack_below_pct: 50,
		max_repack: "10GiB",
	},
	pause_floor: "24h",
	pause_effective: "24h",
	schedule: null,
	preflight: { problems: [], dry_run_allowed: true, checked_at: Date.now() },
	has_object_lock: false,
	cycle: null,
	last_cleanup: null,
	last_dry_run: null,
	last_aborted_at: null,
	events: [],
	events_total: 0,
	...over,
});

export function useRepository() {
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
	);
}

export function openRepository(initial: GcStatus = gcStatus(), path = "") {
	const current = { status: initial };
	useRepository();
	server.use(
		http.get("/api/v1/repositories/repo-1/gc", () =>
			HttpResponse.json(current.status),
		),
	);
	renderApp(`/settings/repositories/repo-1${path}`);
	return current;
}

export const card = async () =>
	within(
		(await screen.findByRole("heading", { name: "Free up space" })).closest(
			"[data-slot=card]",
		) as HTMLElement,
	);
