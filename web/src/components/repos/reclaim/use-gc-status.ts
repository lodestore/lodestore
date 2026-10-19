import { useGetStatus } from "@/api/gen/endpoints";
import type { GcSettingsInput, GcStatus } from "@/api/gen/model";

export const STATUS_ID = "reclaim-status";

export const ACTIVE_POLL_MS = 2_000;
export const WAITING_POLL_MS = 15_000;
export const IDLE_POLL_MS = 30_000;

export function gcPollMs(status?: GcStatus) {
	if (status?.active_dry_run) return ACTIVE_POLL_MS;
	const cycle = status?.cycle;
	if (!cycle) return IDLE_POLL_MS;
	if (cycle.dry_run) return ACTIVE_POLL_MS;
	return cycle.state === "waiting" ? WAITING_POLL_MS : ACTIVE_POLL_MS;
}

export function useGcStatus(repositoryId: string) {
	return useGetStatus(repositoryId, {
		query: {
			refetchInterval: (query) => gcPollMs(query.state.data),
			refetchIntervalInBackground: false,
		},
	});
}

export type GcFailure = { reason: string; afterDelete: boolean };

export function gcFailure(status: GcStatus): GcFailure | null {
	const failure = status.last_failure;
	if (!failure) return null;
	return { reason: failure.error, afterDelete: failure.after_delete };
}

export const dryRunActive = (status: GcStatus) =>
	status.active_dry_run != null || status.cycle?.dry_run === true;

export const settingsInput = (
	status: GcStatus,
	enabled: boolean,
): GcSettingsInput => ({ ...status.settings, enabled });
