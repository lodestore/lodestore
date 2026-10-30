import { HttpResponse, http } from "msw";
import type {
	AgentUpdateStatus,
	HostUpdate,
	Rollout,
	RolloutRollback,
	RolloutWave,
} from "@/api/gen/model";
import { server } from "@/test/server";

export const AGENT_DIR = "/usr/share/lodestore/agents";

export function hostUpdate(
	id: string,
	over: Partial<HostUpdate> = {},
): HostUpdate {
	return {
		id,
		hostname: id,
		arch: "x86_64",
		version: "0.1.0",
		outdated: false,
		updatable: false,
		...over,
	};
}

export function rollout(over: Partial<Rollout> = {}): Rollout {
	return {
		id: "rollout-1",
		target_version: "0.2.0",
		state: "running",
		canary: 2,
		wave: 10,
		pause_after: 1,
		total: 4,
		updated: 1,
		rolled_back: 0,
		rollbacks_since_resume: 0,
		waves: [],
		rollbacks: [],
		created_at: Date.now() - 60_000,
		created_by: "admin",
		...over,
	};
}

export const wave = (
	index: number,
	total: number,
	updated: number,
	rolledBack: number,
	waiting: number,
): RolloutWave => ({
	wave: index,
	total,
	updated,
	rolled_back: rolledBack,
	waiting,
});

export const rollback = (
	hostname: string,
	reason: string,
): RolloutRollback => ({
	update_id: `${hostname}-update`,
	host_id: `${hostname}-0000-4000-8000-000000000000`,
	hostname,
	reason,
	finished_at: Date.now() - 30_000,
});

export function agentUpdateStatus(
	hosts: HostUpdate[] = [],
	over: Partial<AgentUpdateStatus> = {},
): AgentUpdateStatus {
	return {
		agent_dir: AGENT_DIR,
		binaries: [],
		signed: true,
		target_version: "0.2.0",
		settings: { canary: 2, wave: 10, pause_after: 1, rollback_after_secs: 120 },
		hosts,
		rollout: null,
		...over,
	};
}

export function mockAgentUpdates(
	status: AgentUpdateStatus | (() => AgentUpdateStatus),
) {
	server.use(
		http.get("/api/v1/agent-updates", () =>
			HttpResponse.json(typeof status === "function" ? status() : status),
		),
	);
}
