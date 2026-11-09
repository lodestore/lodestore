/// <reference types="node" />
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const ROOT = join(process.cwd(), "..");
const read = (path: string) => readFileSync(join(ROOT, path), "utf8");

const [comment, key] = read("crates/lodestore-agent/keys/project.pub")
	.trim()
	.split("\n");
const keyId = Array.from(Buffer.from(key, "base64").subarray(2, 10))
	.reverse()
	.map((b) => b.toString(16).padStart(2, "0"))
	.join("")
	.toUpperCase();

describe("project key", () => {
	it("has the key id written in project.pub", () => {
		expect(comment).toBe(`untrusted comment: minisign public key ${keyId}`);
	});

	it.each([
		[
			"the installer help",
			"crates/lodestore-server/src/install/install.sh",
			keyId,
		],
		["the Binary page", "docs/src/content/docs/docs/install/binary.md", keyId],
		[
			"the threat model",
			"docs/src/content/docs/docs/security/threat-model.md",
			keyId,
		],
		["the quickstart", "docs/src/content/docs/docs/quickstart.md", key],
	])("is the one shown in %s", (_name, path, needle) => {
		expect(read(path)).toContain(needle);
	});
});
