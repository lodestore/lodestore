import { useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { toast } from "sonner";
import { ApiError } from "@/api/fetcher";
import {
	getAgentUpdateStatusQueryKey,
	getListHostsQueryKey,
	getListJobsQueryKey,
	useAgentUpdateStatus,
	useCancelRollout,
	useDismissRollout,
	usePauseRollout,
	useResumeRollout,
	useStartRollout,
} from "@/api/gen/endpoints";
import type {
	AgentUpdateStatus,
	HostUpdate,
	StartRollout,
} from "@/api/gen/model";
import { copy } from "@/copy";

const ACTIVE_MS = 5_000;
const IDLE_MS = 30_000;

function isBusy(status?: AgentUpdateStatus) {
	if (!status) return false;
	const state = status.rollout?.state;
	return (
		state === "running" ||
		state === "paused" ||
		status.hosts.some((h) => h.state === "updating" || h.state === "queued")
	);
}

export function useRollout() {
	const queryClient = useQueryClient();
	const query = useAgentUpdateStatus({
		query: {
			refetchInterval: (q) => (isBusy(q.state.data) ? ACTIVE_MS : IDLE_MS),
			refetchIntervalInBackground: false,
		},
	});
	const status = query.data;
	const byHost = useMemo(
		() => new Map<string, HostUpdate>(status?.hosts.map((h) => [h.id, h])),
		[status],
	);

	const refresh = () => {
		for (const queryKey of [
			getAgentUpdateStatusQueryKey(),
			getListHostsQueryKey(),
			getListJobsQueryKey(),
		]) {
			void queryClient.invalidateQueries({ queryKey });
		}
	};
	const reloadStatus = () =>
		void queryClient.invalidateQueries({
			queryKey: getAgentUpdateStatusQueryKey(),
		});
	const onError = (error: Error) => {
		toast.error(error.message);
		reloadStatus();
	};
	const onStartError = (error: Error) => {
		if (error instanceof ApiError && error.status === 409) {
			toast.error(copy.agentUpdate.conflict);
			reloadStatus();
			return;
		}
		toast.error(error.message);
	};
	const mutation = { onSuccess: refresh, onError };

	const startMutation = useStartRollout({
		mutation: { onSuccess: refresh, onError: onStartError },
	});
	const pauseMutation = usePauseRollout({ mutation });
	const resumeMutation = useResumeRollout({ mutation });
	const cancelMutation = useCancelRollout({ mutation });
	const dismissMutation = useDismissRollout({ mutation });

	return {
		status,
		isError: query.isError,
		refetch: query.refetch,
		byHost,
		starting: startMutation.isPending,
		acting:
			pauseMutation.isPending ||
			resumeMutation.isPending ||
			cancelMutation.isPending ||
			dismissMutation.isPending,
		start: (data: StartRollout, onStarted?: () => void) =>
			startMutation.mutate({ data }, { onSuccess: onStarted }),
		pause: (id: string) => pauseMutation.mutate({ id }),
		resume: (id: string) => resumeMutation.mutate({ id }),
		cancel: (id: string) => cancelMutation.mutate({ id }),
		dismiss: (id: string) => dismissMutation.mutate({ id }),
	};
}
