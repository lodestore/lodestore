import { type QueryClient, queryOptions } from "@tanstack/react-query";
import { redirect } from "@tanstack/react-router";
import { ApiError, setCsrfToken } from "@/api/fetcher";
import {
	getInfoQueryOptions,
	getListRepositoriesQueryOptions,
	getSessionQueryOptions,
	getStatusQueryOptions,
	session,
} from "@/api/gen/endpoints";

export const sessionQuery = queryOptions({
	...getSessionQueryOptions({ query: { retry: false, staleTime: Infinity } }),
	queryFn: async ({ signal }) => {
		const current = await session(undefined, signal);
		setCsrfToken(current.csrf_token);
		return current;
	},
});

export const statusQuery = getStatusQueryOptions({
	query: { retry: false, staleTime: 30_000 },
});

export const infoQuery = getInfoQueryOptions({
	query: { staleTime: 5 * 60_000 },
});

export const repositoriesQuery = getListRepositoriesQueryOptions({
	query: { staleTime: 30_000 },
});

export function isSetupRequired(error: unknown) {
	return (
		error instanceof ApiError &&
		error.status === 409 &&
		error.code === "setup_required"
	);
}

export function isSignedOut(error: unknown) {
	return (
		(error instanceof ApiError && error.status === 401) ||
		isSetupRequired(error)
	);
}

export async function requireSession(
	queryClient: QueryClient,
	location: { href: string },
) {
	const toLogin = () =>
		redirect({
			to: "/login",
			search: { next: location.href, expired: undefined },
		});
	if (queryClient.getQueryData(sessionQuery.queryKey) === undefined) {
		const status = await queryClient
			.fetchQuery({ ...statusQuery, staleTime: 0 })
			.catch(() => null);
		if (status?.setup_required) throw toLogin();
	}
	try {
		await queryClient.ensureQueryData(sessionQuery);
	} catch (error) {
		if (isSignedOut(error)) throw toLogin();
		throw error;
	}
}

export async function redirectIfSignedIn(queryClient: QueryClient) {
	try {
		const status = await queryClient.fetchQuery(statusQuery);
		if (status.setup_required) return;
	} catch (error) {
		console.debug("Could not read the server status", error);
	}
	const cached = queryClient.getQueryState(sessionQuery.queryKey);
	if (
		isSignedOut(cached?.error) &&
		Date.now() - (cached?.errorUpdatedAt ?? 0) < 5_000
	) {
		return;
	}
	try {
		await queryClient.fetchQuery(sessionQuery);
	} catch {
		return;
	}
	throw redirect({ to: "/hosts" });
}

export function safeNext(next: unknown) {
	return typeof next === "string" && /^\/(?![/\\])\P{Cc}*$/u.test(next)
		? next
		: undefined;
}
