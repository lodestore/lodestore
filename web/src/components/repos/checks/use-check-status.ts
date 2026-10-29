import { useGetCheckStatus } from "@/api/gen/endpoints";
import type { CheckLevelStatus, CheckStatus } from "@/api/gen/model";

export const ACTIVE_POLL_MS = 2_000;
export const IDLE_POLL_MS = 30_000;

export const checkActive = (status?: CheckStatus) =>
	status?.levels.some((level) => level.running != null) ?? false;

export function useCheckStatus(repositoryId: string) {
	return useGetCheckStatus(repositoryId, {
		query: {
			refetchInterval: (query) =>
				checkActive(query.state.data) ? ACTIVE_POLL_MS : IDLE_POLL_MS,
			refetchIntervalInBackground: false,
		},
	});
}

export const levelFailed = (level: CheckLevelStatus) =>
	level.last != null &&
	(level.last.outcome === "problems" ||
		level.last.outcome === "stopped" ||
		level.last.state === "failed");
