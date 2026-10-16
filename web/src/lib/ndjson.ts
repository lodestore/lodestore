import { ApiError, notifyUnauthorized } from "@/api/fetcher";

/** Streams an NDJSON response, handing over the lines of each chunk at once. */
export async function streamNdjson<T>(
	url: string,
	signal: AbortSignal,
	onLines: (lines: T[]) => void,
) {
	const response = await fetch(url, { credentials: "same-origin", signal });
	if (!response.ok || !response.body) {
		if (response.status === 401) notifyUnauthorized();
		const body = (await response.json().catch(() => null)) as {
			code?: string;
			message?: string;
		} | null;
		throw new ApiError(
			response.status,
			body?.code ?? "unknown",
			body?.message ?? response.statusText,
		);
	}
	const reader = response.body.getReader();
	const decoder = new TextDecoder();
	let pending = "";
	const emit = (texts: string[]) => {
		const lines = texts.filter((text) => text.trim());
		if (lines.length > 0) onLines(lines.map((text) => JSON.parse(text) as T));
	};
	try {
		for (;;) {
			const { done, value } = await reader.read();
			if (done) break;
			pending += decoder.decode(value, { stream: true });
			const lines = pending.split("\n");
			pending = lines.pop() ?? "";
			emit(lines);
		}
		emit([pending + decoder.decode()]);
	} finally {
		reader.cancel().catch(() => {});
	}
}
