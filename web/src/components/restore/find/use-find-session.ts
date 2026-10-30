import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo } from "react";
import { cancelFind, getFind, startFind } from "@/api/gen/endpoints";
import type { FindGroup, FindRequest, FindSession } from "@/api/gen/model";

const POLL_MS = 1000;
const ACTIVE = ["find", "active"] as const;
const sessionKey = (id: string | undefined) => ["find", "session", id] as const;

export type FindFilters = {
	hosts?: string;
	labels?: string;
	since?: number;
	until?: number;
};

const list = (value: string | undefined) =>
	value?.split(",").filter(Boolean) ?? [];

export function findRequest(
	pattern: string | undefined,
	filters: FindFilters = {},
): FindRequest | undefined {
	if (!pattern) return undefined;
	const hosts = list(filters.hosts);
	const labels = list(filters.labels);
	return {
		pattern,
		...(hosts.length > 0 && { hosts }),
		...(labels.length > 0 && { labels }),
		...(filters.since !== undefined && { since: filters.since }),
		...(filters.until !== undefined && { until: filters.until }),
	};
}

export const findKey = (group: FindGroup) =>
	`${group.host_key}/${group.path_b64}`;

function merge(previous: FindSession | undefined, next: FindSession) {
	const byIndex = new Map<number, FindGroup>();
	for (const group of previous?.groups ?? []) byIndex.set(group.index, group);
	for (const group of next.groups) byIndex.set(group.index, group);
	return { ...next, groups: [...byIndex.values()] };
}

/**
 * The session lives in the query cache, so leaving the tab and coming back
 * neither restarts the search nor loses the groups; a different request
 * cancels the one still running.
 */
export function useFindSession(request: FindRequest | undefined, run?: number) {
	const queryClient = useQueryClient();

	const started = useQuery({
		queryKey: ["find", "start", request, run],
		enabled: request !== undefined,
		staleTime: Number.POSITIVE_INFINITY,
		retry: false,
		queryFn: async () => {
			const previous = queryClient.getQueryData<string>(ACTIVE);
			const state = queryClient.getQueryData<FindSession>(
				sessionKey(previous),
			)?.state;
			if (previous && (state === undefined || state === "running")) {
				await cancelFind(previous).catch(() => {});
			}
			const { id } = await startFind(request as FindRequest);
			queryClient.setQueryData(ACTIVE, id);
			return id;
		},
	});

	const id = started.data;
	const session = useQuery({
		queryKey: sessionKey(id),
		enabled: id !== undefined,
		staleTime: Number.POSITIVE_INFINITY,
		retry: false,
		refetchInterval: (query) =>
			query.state.data?.state === "running" ? POLL_MS : false,
		queryFn: async () => {
			const previous = queryClient.getQueryData<FindSession>(sessionKey(id));
			const next = await getFind(id as string, {
				after: previous?.revision ?? 0,
			});
			return merge(previous, next);
		},
	});

	const { refetch: restart } = started;
	const state = session.data?.state;
	useEffect(() => {
		// Back to a search another one cancelled: start it again.
		if (id !== undefined && state === "cancelled") {
			if (queryClient.getQueryData(ACTIVE) !== id) void restart();
		}
	}, [id, state, queryClient, restart]);

	const groups = useMemo(
		() =>
			[...(session.data?.groups ?? [])].sort(
				(a, b) =>
					a.hostname.localeCompare(b.hostname) ||
					a.host_key.localeCompare(b.host_key) ||
					a.path.localeCompare(b.path),
			),
		[session.data?.groups],
	);

	const stop = useCallback(async () => {
		if (id === undefined) return;
		await cancelFind(id).catch(() => {});
		await queryClient.invalidateQueries({ queryKey: sessionKey(id) });
	}, [id, queryClient]);

	const retry = useCallback(() => void restart(), [restart]);

	return {
		session: session.data,
		groups,
		stop,
		error: started.error ?? session.error,
		retry,
	};
}
