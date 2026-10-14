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

export async function redirectIfSignedIn(queryClient: QueryClient) {
	try {
		const status = await queryClient.fetchQuery(statusQuery);
		if (status.setup_required) return;
	} catch (error) {
		console.debug("Could not read the server status", error);
	}
	try {
		await queryClient.fetchQuery(sessionQuery);
	} catch {
		return;
	}
	throw redirect({ to: "/hosts" });
}

export function safeNext(next: unknown) {
	return typeof next === "string" &&
		next.startsWith("/") &&
		!next.startsWith("//") &&
		!next.startsWith("/\\")
		? next
		: undefined;
}
