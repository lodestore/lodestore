import type { RepositoryDetail, StoragePresetDto } from "@/api/gen/model";

const STORAGE_BOX = ".your-storagebox.de";

function hostOf(endpoint: string) {
	return endpoint
		.replace(/^[a-z+]+:\/\//i, "")
		.replace(/[/:].*$/, "")
		.toLowerCase();
}

function matchesTemplate(template: string, endpoint: string) {
	const pattern = template
		.split(/\{\w+\}/)
		.map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
		.join("[^./]+");
	return new RegExp(`^${pattern}/?$`, "i").test(endpoint);
}

export function presetOf(
	repository: Pick<RepositoryDetail, "backend" | "location">,
	presets: StoragePresetDto[],
): StoragePresetDto | undefined {
	const byId = (id: string) => presets.find((p) => p.id === id);
	const options = repository.location.options ?? {};
	switch (repository.backend) {
		case "azblob":
			return byId("azure-blob");
		case "gcs":
			return byId("google-cloud-storage");
		case "sftp":
			return hostOf(options.endpoint ?? "").endsWith(STORAGE_BOX)
				? byId("hetzner-storage-box")
				: byId("sftp");
		case "s3": {
			if (options.provider === "b2") return byId("backblaze-b2");
			const endpoint = (options.endpoint ?? "").trim();
			if (endpoint === "") return byId("aws-s3");
			return (
				presets.find(
					(p) =>
						p.backend === "s3" &&
						p.endpoint_template &&
						matchesTemplate(p.endpoint_template, endpoint),
				) ?? byId("minio")
			);
		}
		default:
			return undefined;
	}
}
