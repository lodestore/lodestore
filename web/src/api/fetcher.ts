export class ApiError extends Error {
	readonly status: number;
	readonly code: string;
	readonly retryAfter?: number;

	constructor(
		status: number,
		code: string,
		message: string,
		retryAfter?: number,
	) {
		super(message);
		this.name = "ApiError";
		this.status = status;
		this.code = code;
		this.retryAfter = retryAfter;
	}
}

export type ErrorType<E> = E extends unknown ? ApiError : never;
export type BodyType<B> = B;

type RequestConfig = {
	url: string;
	method: string;
	params?: Record<string, unknown>;
	data?: unknown;
	headers?: Record<string, string>;
	signal?: AbortSignal;
};

const MUTATING = new Set(["POST", "PUT", "PATCH", "DELETE"]);
const AUTH_PATHS = ["/api/v1/auth/login", "/api/v1/auth/session"];

let csrfToken: string | undefined;
let unauthorizedHandler: (() => void) | undefined;

export function setCsrfToken(token: string | undefined) {
	csrfToken = token;
}

export function onUnauthorized(handler: (() => void) | undefined) {
	unauthorizedHandler = handler;
}

function withParams(url: string, params?: Record<string, unknown>) {
	if (!params) return url;
	const search = new URLSearchParams();
	for (const [key, value] of Object.entries(params)) {
		if (value !== undefined && value !== null) search.set(key, String(value));
	}
	const query = search.toString();
	return query ? `${url}?${query}` : url;
}

export async function apiFetch<T>(
	{ url, method, params, data, headers, signal }: RequestConfig,
	options?: RequestInit,
): Promise<T> {
	const verb = method.toUpperCase();
	const merged = new Headers(options?.headers);
	for (const [name, value] of Object.entries(headers ?? {})) {
		merged.set(name, value);
	}
	if (data !== undefined) merged.set("Content-Type", "application/json");
	if (MUTATING.has(verb) && csrfToken) merged.set("X-CSRF-Token", csrfToken);

	const response = await fetch(withParams(url, params), {
		...options,
		method: verb,
		headers: merged,
		credentials: "same-origin",
		body: data === undefined ? undefined : JSON.stringify(data),
		signal: signal ?? options?.signal,
	});

	if (!response.ok) {
		const body = (await response.json().catch(() => null)) as {
			code?: string;
			message?: string;
		} | null;
		const retryAfter = Number(response.headers.get("Retry-After"));
		const error = new ApiError(
			response.status,
			body?.code ?? "unknown",
			body?.message ?? response.statusText,
			Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter : undefined,
		);
		if (response.status === 401 && !AUTH_PATHS.includes(url)) {
			unauthorizedHandler?.();
		}
		throw error;
	}

	if (response.status === 204) return undefined as T;
	return (await response.json()) as T;
}
