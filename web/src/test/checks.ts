import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type {
	CheckFinding,
	CheckLevel,
	CheckLevelStatus,
	CheckRunDto,
	CheckStatus,
	ScheduleInfo,
} from "@/api/gen/model";
import { renderApp } from "./app";
import { useRepository } from "./gc";
import { server } from "./server";

export const checkRun = (over: Partial<CheckRunDto> = {}): CheckRunDto => ({
	id: "run-1",
	level: "structure",
	origin: "schedule",
	state: "done",
	outcome: "passed",
	created_at: Date.now() - 2 * 3_600_000,
	started_at: Date.now() - 2 * 3_600_000,
	finished_at: Date.now() - 2 * 3_600_000 + 42_000,
	duration_ms: 42_000,
	started_by: "admin",
	error: null,
	progress: null,
	findings: [],
	findings_total: 0,
	summary: {
		structure: {
			snapshots_checked: 12,
			trees_checked: 3400,
			packs_listed: 810,
			missing_packs: 0,
		},
		packs: null,
	},
	...over,
});

export const packsRun = (over: Partial<CheckRunDto> = {}): CheckRunDto =>
	checkRun({
		id: "run-2",
		level: "packs",
		summary: {
			structure: null,
			packs: {
				packs_read: 40,
				bytes_read: 5 * 1024 ** 3,
				blobs_verified: 120_000,
				damaged_blobs: 0,
			},
		},
		...over,
	});

export const restoreRun = (over: Partial<CheckRunDto> = {}): CheckRunDto =>
	checkRun({
		id: "run-4",
		level: "restore",
		summary: {
			structure: null,
			packs: null,
			restore: {
				snapshots_sampled: 3,
				files_restored: 20,
				bytes_restored: 12 * 1024 ** 2,
				hash_mismatches: 0,
				skipped: 0,
				stopped_at_limit: false,
			},
		},
		...over,
	});

export const finding = (i: number): CheckFinding => ({
	kind: "damaged_blob",
	key: `packs/ab/abcd${i}`,
	detail: `blob hash mismatch in packs/ab/abcd${i}`,
});

export const failedPacksRun = (total = 25): CheckRunDto =>
	packsRun({
		state: "failed",
		outcome: "problems",
		findings: Array.from({ length: Math.min(total, 100) }, (_, i) =>
			finding(i),
		),
		findings_total: total,
	});

export const scheduleInfo = (summary: string): ScheduleInfo => ({
	summary,
	timezone: "UTC",
	next_runs: [Date.now() + 3_600_000],
	jitter_secs: 0,
	catch_up_secs: 0,
	dst_notes: [],
});

export const levelStatus = (
	level: CheckLevel,
	over: Partial<CheckLevelStatus> = {},
): CheckLevelStatus => ({
	level,
	last: null,
	running: null,
	schedule: null,
	settings: {
		cron:
			level === "structure"
				? "0 2 * * *"
				: level === "packs"
					? "0 4 * * 3"
					: "0 5 2 * *",
		timezone: "UTC",
		enabled: true,
		read_per_run: "5GiB",
		files_per_run: 20,
		max_data_per_run: "1GiB",
	},
	...over,
});

export const checkStatus = (over: Partial<CheckStatus> = {}): CheckStatus => ({
	repository_id: "repo-1",
	levels: [
		levelStatus("structure"),
		levelStatus("packs"),
		levelStatus("restore"),
	],
	usage: null,
	coverage: null,
	blocked: null,
	...over,
});

export function openChecks(initial: CheckStatus = checkStatus()) {
	const current = { status: initial };
	useRepository();
	server.use(
		http.get("/api/v1/repositories/repo-1/checks", () =>
			HttpResponse.json(current.status),
		),
		http.get("/api/v1/repositories/repo-1/gc", () =>
			HttpResponse.json({}, { status: 500 }),
		),
	);
	renderApp("/settings/repositories/repo-1");
	return current;
}

export const checksCard = async () =>
	within(
		(await screen.findByRole("heading", { name: "Integrity checks" })).closest(
			"[data-slot=card]",
		) as HTMLElement,
	);
