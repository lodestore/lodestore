import { HttpResponse, http } from "msw";
import type {
	FindGroup,
	FindSession,
	FindSnapshotRef,
	FindVersion,
} from "@/api/gen/model";
import { encodeB64 } from "@/lib/names";
import { server } from "@/test/server";

export const DAY = 86_400_000;

export const findId = (n: number) =>
	`${String(n).padStart(8, "b")}${"0".repeat(56)}`;

export function findSnapshotRef(
	n: number,
	over: Partial<FindSnapshotRef> = {},
): FindSnapshotRef {
	return { id: findId(n), time_ms: Date.UTC(2026, 9, 21, 2, 0), ...over };
}

export function findVersion(over: Partial<FindVersion> = {}): FindVersion {
	return {
		content_id: "c41e00000000000000000000000000009a07",
		first_ms: Date.UTC(2026, 9, 15, 2, 0),
		last_ms: Date.UTC(2026, 9, 21, 2, 0),
		kind: "file",
		mtime_ms: Date.UTC(2026, 9, 15, 14, 2),
		size: 3174,
		snapshot_count: 2,
		snapshots: [findSnapshotRef(2), findSnapshotRef(1)],
		...over,
	};
}

export function manySnapshots(count: number): FindSnapshotRef[] {
	return Array.from({ length: Math.min(count, 6) }, (_, i) =>
		findSnapshotRef(count - i, {
			time_ms: Date.UTC(2026, 9, 21, 2, 0) - i * DAY,
		}),
	);
}

export function findGroup(
	index: number,
	over: Partial<FindGroup> = {},
): FindGroup {
	const path = over.path ?? "/etc/nginx/nginx.conf";
	return {
		index,
		host_key: `agent-${index}`,
		host_id: `agent-${index}`,
		hostname: `web-${index}`,
		agent: true,
		labels: [{ key: "env", value: "prod" }],
		path,
		path_b64: encodeB64(new TextEncoder().encode(path)),
		last_seen_ms: Date.UTC(2026, 9, 29, 2, 0),
		revision: index + 1,
		versions: [findVersion()],
		gone: null,
		...over,
	};
}

export function findSession(over: Partial<FindSession> = {}): FindSession {
	return {
		id: "find-1",
		pattern: "*.conf",
		state: "running",
		limit: 1000,
		scanned: 1,
		total: 4,
		skipped: 0,
		hosts: 2,
		searched_hosts: 2,
		files: 1,
		versions: 1,
		started_at: Date.UTC(2026, 9, 29, 12, 0),
		revision: 1,
		groups: [],
		...over,
	};
}

/** Answers `GET /find/:id` with the given sessions in order, then repeats the last. */
export function findHandlers(responses: FindSession[], id = "find-1") {
	const calls = { start: 0, get: 0, cancel: 0, bodies: [] as unknown[] };
	const afters: (string | null)[] = [];
	server.use(
		http.post("/api/v1/find", async ({ request }) => {
			calls.start += 1;
			calls.bodies.push(await request.json());
			return HttpResponse.json({ id });
		}),
		http.get("/api/v1/find/:id", ({ request, params }) => {
			if (params.id !== id) return new HttpResponse(null, { status: 404 });
			afters.push(new URL(request.url).searchParams.get("after"));
			const next = responses[Math.min(calls.get, responses.length - 1)];
			calls.get += 1;
			return HttpResponse.json(next);
		}),
		http.delete("/api/v1/find/:id", () => {
			calls.cancel += 1;
			return new HttpResponse(null, { status: 204 });
		}),
	);
	return { calls, afters };
}
