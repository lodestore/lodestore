import { QueryClient } from "@tanstack/react-query";
import { ApiError } from "@/api/fetcher";

export function shouldRetry(count: number, error: unknown) {
	return !(error instanceof ApiError && error.status < 500) && count < 2;
}

export function createQueryClient() {
	return new QueryClient({
		defaultOptions: { queries: { retry: shouldRetry } },
	});
}
