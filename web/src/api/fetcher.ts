import type { FieldError, PreflightProblem } from "@/api/gen/model";

export class ApiError extends Error {
	readonly status: number;
	readonly code: string;
	readonly retryAfter?: number;
	readonly problems?: PreflightProblem[];
	readonly errors?: FieldError[];

	constructor(
		status: number,
		code: string,
		message: string,
		retryAfter?: number,
		details: { problems?: PreflightProblem[]; errors?: FieldError[] } = {},
	) {
		super(message);
		this.name = "ApiError";
		this.status = status;
		this.code = code;
		this.retryAfter = retryAfter;
		this.problems = details.problems;
		this.errors = details.errors;
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
	responseType?: "blob";
};

const MUTATING = new Set(["POST", "PUT", "PATCH", "DELETE"]);
const AUTH_PATHS = [
	"/api/v1/auth/demo",
	"/api/v1/auth/login",
	"/api/v1/auth/logout",
	"/api/v1/auth/session",
];

let csrfToken: string | undefined;
let unauthorizedHandler: (() => void) | undefined;
let demoBlockedHandler: (() => void) | undefined;
let stepUpHandler: (() => Promise<boolean>) | undefined;

export function setCsrfToken(token: string | undefined) {
	csrfToken = token;
}

export function onUnauthorized(handler: (() => void) | undefined) {
	unauthorizedHandler = handler;
}

export function onDemoBlocked(handler: (() => void) | undefined) {
	demoBlockedHandler = handler;
}

export function onStepUpRequired(
	handler: (() => Promise<boolean>) | undefined,
) {
	stepUpHandler = handler;
}

/** Sends the user to login for a 401 from a request that bypasses apiFetch. */
export function notifyUnauthorized() {
	unauthorizedHandler?.();
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
	{ url, method, params, data, headers, signal, responseType }: RequestConfig,
	options?: RequestInit,
	retried = false,
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
			problems?: PreflightProblem[];
			errors?: FieldError[];
		} | null;
		const retryAfter = Number(response.headers.get("Retry-After"));
		const error = new ApiError(
			response.status,
			body?.code ?? "unknown",
			body?.message ??
				body?.errors?.map((e) => e.message).join(" ") ??
				response.statusText,
			Number.isFinite(retryAfter) && retryAfter > 0 ? retryAfter : undefined,
			{ problems: body?.problems, errors: body?.errors },
		);
		if (response.status === 401 && !AUTH_PATHS.includes(url)) {
			unauthorizedHandler?.();
		}
		if (response.status === 403 && body?.code === "demo_mode") {
			demoBlockedHandler?.();
		}
		if (
			response.status === 403 &&
			body?.code === "step_up_required" &&
			!retried &&
			stepUpHandler &&
			(await stepUpHandler())
		) {
			return apiFetch<T>(
				{ url, method, params, data, headers, signal, responseType },
				options,
				true,
			);
		}
		throw error;
	}

	if (response.status === 204) return undefined as T;
	if (responseType === "blob") return (await response.blob()) as T;
	return (await response.json()) as T;
}
