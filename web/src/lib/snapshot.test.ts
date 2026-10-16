import { describe, expect, it } from "vitest";
import { archiveName } from "@/lib/snapshot";

describe("archiveName", () => {
	const id = "0123456789abcdef";

	it("matches the server's file name rule", () => {
		expect(archiveName("web-1", id, "tar.gz")).toBe("web-1-01234567.tar.gz");
		expect(archiveName("héllo 🙂", id, "zip")).toBe("h_llo__-01234567.zip");
		expect(archiveName("", id, "tar.zst")).toBe("snapshot-01234567.tar.zst");
	});
});
