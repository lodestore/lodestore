import { HttpResponse, http } from "msw";
import { setupServer } from "msw/node";

export const emptyFleet = {
	generated_at: 0,
	totals: {
		hosts: 0,
		healthy: 0,
		overdue: 0,
		failed: 0,
		offline: 0,
		not_protected: 0,
	},
	storage: { repositories: 0, counted: 0 },
	hosts: [],
};

type Mechanism = "policy" | "commands" | "instruction" | "server_issued";

function preset(
	id: string,
	label: string,
	backend: string,
	mechanism: Mechanism,
	note: string,
	extra: {
		endpoint_template?: string;
		region?: string;
		path_style?: boolean;
		provider?: string;
		verified?: boolean;
	} = {},
) {
	return {
		id,
		label,
		backend,
		endpoint_template: extra.endpoint_template ?? null,
		region: extra.region ?? null,
		path_style: extra.path_style ?? false,
		provider: extra.provider ?? null,
		mechanism,
		verified: extra.verified ?? false,
		note,
	};
}

export const storagePresets = [
	preset("aws-s3", "Amazon S3", "s3", "policy", "IAM policy without delete."),
	preset(
		"minio",
		"MinIO",
		"s3",
		"commands",
		"mc commands for a user without delete.",
		{ path_style: true, verified: true },
	),
	preset("wasabi", "Wasabi", "s3", "policy", "IAM policy without delete.", {
		endpoint_template: "https://s3.{region}.wasabisys.com",
	}),
	preset(
		"backblaze-b2",
		"Backblaze B2",
		"s3",
		"commands",
		"Application key without deleteFiles.",
		{
			endpoint_template: "https://s3.{region}.backblazeb2.com",
			provider: "b2",
		},
	),
	preset(
		"cloudflare-r2",
		"Cloudflare R2",
		"s3",
		"instruction",
		"A token cannot be limited; use a bucket lock.",
		{
			endpoint_template: "https://{account_id}.r2.cloudflarestorage.com",
			region: "auto",
		},
	),
	preset(
		"hetzner-object-storage",
		"Hetzner Object Storage",
		"s3",
		"policy",
		"Bucket policy for a key from another project.",
		{ endpoint_template: "https://{region}.your-objectstorage.com" },
	),
	preset(
		"hetzner-storage-box",
		"Hetzner Storage Box",
		"sftp",
		"instruction",
		"A sub-account with the repository directory as its base.",
		{ endpoint_template: "ssh://{user}.your-storagebox.de:23" },
	),
	preset(
		"azure-blob",
		"Azure Blob Storage",
		"azblob",
		"server_issued",
		"The server issues a SAS without delete.",
	),
	preset(
		"google-cloud-storage",
		"Google Cloud Storage",
		"gcs",
		"commands",
		"Custom role without delete.",
	),
	preset(
		"sftp",
		"SFTP (OpenSSH)",
		"sftp",
		"commands",
		"Chrooted user with a deny list.",
		{ verified: true },
	),
];

export function rightsFor(id: string, bucket = "") {
	const found = storagePresets.find((p) => p.id === id) ?? storagePresets[0];
	return {
		mechanism: found.mechanism,
		note: found.note,
		preset: found.id,
		server_text: null,
		text: `agent-rights ${found.id} ${bucket}`.trim(),
		verified: found.verified,
	};
}

export const server = setupServer(
	http.get("/api/v1/storage-presets", () => HttpResponse.json(storagePresets)),
	http.get("/api/v1/agent-policy", ({ request }) => {
		const params = new URL(request.url).searchParams;
		return HttpResponse.json({
			iam_json: "{}",
			minio_commands: "mc",
			rights: rightsFor(
				params.get("preset") ?? "aws-s3",
				params.get("bucket") ?? "",
			),
		});
	}),
	http.get("/api/v1/repositories/:id/storage-access", () =>
		HttpResponse.json({
			after_revoke: "until_rotation",
			status: "no_revoked_hosts",
			rotate_via: "agent-key",
			access_ends_by: null,
			hosts_need_agent_update: 0,
			revoked_hosts_before_rotation: 0,
			revoked_hosts_since_rotation: 0,
		}),
	),
	http.get("/api/v1/agent-updates", () =>
		HttpResponse.json({
			agent_dir: "/usr/share/lodestore/agents",
			binaries: [],
			signed: true,
			target_version: null,
			settings: {
				canary: 2,
				wave: 10,
				pause_after: 1,
				rollback_after_secs: 120,
			},
			hosts: [],
			rollout: null,
		}),
	),
	http.get("/api/v1/catalog", () => HttpResponse.json([])),
	http.get("/api/v1/fleet", () =>
		HttpResponse.json({ ...emptyFleet, generated_at: Date.now() }),
	),
	http.get("/api/v1/fleet/hosts/:id", () =>
		HttpResponse.json(
			{ code: "not_found", message: "host not found" },
			{ status: 404 },
		),
	),
	http.get("/api/v1/alerts", () => HttpResponse.json({ alerts: [], total: 0 })),
	http.get("/api/v1/alerts/count", () =>
		HttpResponse.json({ active: 0, unsilenced: 0 }),
	),
	http.get("/api/v1/repositories/:id/checks", ({ params }) =>
		HttpResponse.json({
			repository_id: params.id,
			levels: [],
			usage: null,
			coverage: null,
			blocked: null,
		}),
	),
	http.get("/api/v1/notifications/channels", () => HttpResponse.json([])),
	http.get("/api/v1/account", () =>
		HttpResponse.json({ username: "admin", email: null }),
	),
);
