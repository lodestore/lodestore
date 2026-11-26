import { describe, expect, it } from "vitest";
import type { RepositoryDetail } from "@/api/gen/model";
import { storagePresets } from "@/test/server";
import { presetOf } from "./presets";

const idOf = (backend: string, options: Record<string, string> = {}) =>
	presetOf(
		{ backend, location: { backend, options } } as Pick<
			RepositoryDetail,
			"backend" | "location"
		>,
		storagePresets,
	)?.id;

describe("presetOf", () => {
	it("maps the single-provider backends", () => {
		expect(idOf("azblob", { container: "c" })).toBe("azure-blob");
		expect(idOf("gcs", { bucket: "b" })).toBe("google-cloud-storage");
	});

	it("tells a Storage Box from another SFTP server by its host", () => {
		expect(idOf("sftp", { endpoint: "ssh://u1.your-storagebox.de:23" })).toBe(
			"hetzner-storage-box",
		);
		expect(idOf("sftp", { endpoint: "ssh://backup.example.com:22" })).toBe(
			"sftp",
		);
		expect(
			idOf("sftp", { endpoint: "ssh://x.your-storagebox.de.evil.io" }),
		).toBe("sftp");
	});

	it("takes Backblaze B2 by provider", () => {
		expect(
			idOf("s3", {
				provider: "b2",
				endpoint: "https://s3.eu-central-003.backblazeb2.com",
			}),
		).toBe("backblaze-b2");
	});

	it("matches an endpoint template with one host segment per placeholder", () => {
		expect(
			idOf("s3", { endpoint: "https://s3.eu-central-1.wasabisys.com" }),
		).toBe("wasabi");
		expect(
			idOf("s3", { endpoint: "https://abc123.r2.cloudflarestorage.com" }),
		).toBe("cloudflare-r2");
		expect(
			idOf("s3", { endpoint: "https://fsn1.your-objectstorage.com/" }),
		).toBe("hetzner-object-storage");
	});

	it("treats an empty endpoint as Amazon S3 and an unknown one as MinIO", () => {
		expect(idOf("s3", { bucket: "b" })).toBe("aws-s3");
		expect(idOf("s3", { endpoint: "https://minio.lan:9000" })).toBe("minio");
		expect(idOf("s3", { endpoint: "https://a.b.wasabisys.com" })).toBe("minio");
	});

	it("has no preset for a local path", () => {
		expect(idOf("local", { path: "/srv" })).toBeUndefined();
	});
});
