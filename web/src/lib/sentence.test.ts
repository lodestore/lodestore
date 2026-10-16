import { describe, expect, it } from "vitest";
import { copy } from "@/copy";
import { sentence } from "@/lib/sentence";

describe("sentence", () => {
	it("ends a server message with exactly one period", () => {
		expect(sentence("Select at least one file or folder.")).toBe(
			"Select at least one file or folder. ",
		);
		expect(sentence("the restore spec is invalid: chunk key")).toBe(
			"The restore spec is invalid: chunk key. ",
		);
		expect(sentence("  ")).toBe("");
		expect(sentence(null)).toBe("");
	});

	it("keeps error copy readable", () => {
		expect(
			copy.errors.submit(
				"start the download",
				"Select at least one file or folder.",
			),
		).toBe(
			"Couldn't start the download. Select at least one file or folder. Try again.",
		);
		expect(
			copy.jobs.errors.restoreFailed("the restore spec is invalid: chunk key"),
		).toBe(
			"The restore failed. The restore spec is invalid: chunk key. Check the log, then restore again.",
		);
	});
});
