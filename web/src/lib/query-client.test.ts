import { describe, expect, it } from "vitest";
import { ApiError } from "@/api/fetcher";
import { shouldRetry } from "./query-client";

describe("shouldRetry", () => {
	it("never retries a client error", () => {
		expect(shouldRetry(0, new ApiError(404, "not_found", "gone"))).toBe(false);
		expect(shouldRetry(0, new ApiError(429, "rate_limited", "slow"))).toBe(
			false,
		);
	});

	it("retries server and network failures twice", () => {
		const server = new ApiError(503, "unavailable", "down");
		expect(shouldRetry(0, server)).toBe(true);
		expect(shouldRetry(1, new TypeError("network"))).toBe(true);
		expect(shouldRetry(2, server)).toBe(false);
	});
});
