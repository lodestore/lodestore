import type { CheckResult, RepositoryDraft } from "@/api/gen/model";
import {
	CHECK_IDS,
	type CheckRow,
	toCheckRows,
} from "@/components/capability-checklist";

export const STEPS = ["storage", "access", "key", "host"] as const;
export type Step = (typeof STEPS)[number];

export function parseStep(value: unknown): Step | undefined {
	return STEPS.find((step) => step === value);
}

export const BACKENDS = [
	"s3",
	"local",
	"azblob",
	"gcs",
	"sftp",
	"webdav",
] as const;
export type Backend = (typeof BACKENDS)[number];

export const EXPERIMENTAL_BACKENDS = [
	"azblob",
	"gcs",
	"sftp",
	"webdav",
] as const satisfies readonly Backend[];

export function isExperimental(backend: Backend) {
	return (EXPERIMENTAL_BACKENDS as readonly Backend[]).includes(backend);
}

export type KeyPair = { id: string; secret: string };
export type OptionRow = { key: string; value: string };

export type Draft = {
	name: string;
	backend: Backend;
	endpoint: string;
	region: string;
	bucket: string;
	prefix: string;
	pathStyle: boolean | null;
	path: string;
	options: OptionRow[];
	serverKey: KeyPair;
	agentKey: KeyPair;
};

const emptyKey = (): KeyPair => ({ id: "", secret: "" });

export const DEFAULT_NAME = "default";
export const NAME_PATTERN = /^[a-z0-9-]{1,48}$/;

export function newDraft(): Draft {
	return {
		name: DEFAULT_NAME,
		backend: "s3",
		endpoint: "",
		region: "",
		bucket: "",
		prefix: "",
		pathStyle: null,
		path: "",
		options: [{ key: "", value: "" }],
		serverKey: emptyKey(),
		agentKey: emptyKey(),
	};
}

export function withoutSecrets(draft: Draft): Draft {
	return {
		...draft,
		options: [{ key: "", value: "" }],
		serverKey: emptyKey(),
		agentKey: emptyKey(),
	};
}

export function resetBackend(draft: Draft, backend: Backend): Draft {
	return { ...newDraft(), name: draft.name, backend };
}

export function toApiDraft(draft: Draft): RepositoryDraft {
	const options: Record<string, string> = {};
	const put = (key: string, value: string) => {
		const trimmed = value.trim();
		if (trimmed !== "") options[key] = trimmed;
	};
	if (draft.backend === "s3") {
		put("bucket", draft.bucket);
		put("region", draft.region);
		put("endpoint", draft.endpoint);
		put("prefix", draft.prefix);
		if (draft.pathStyle !== null) options.path_style = String(draft.pathStyle);
		return {
			name: draft.name,
			location: { backend: "s3", options },
			server_credentials: {
				access_key_id: draft.serverKey.id.trim(),
				secret_access_key: draft.serverKey.secret,
			},
			agent_credentials: {
				access_key_id: draft.agentKey.id.trim(),
				secret_access_key: draft.agentKey.secret,
			},
		};
	}
	if (draft.backend === "local") {
		put("path", draft.path);
	} else {
		for (const row of draft.options) put(row.key.trim(), row.value);
	}
	return { name: draft.name, location: { backend: draft.backend, options } };
}

export function accessReady(draft: Draft) {
	switch (draft.backend) {
		case "s3":
			return (
				draft.bucket.trim() !== "" &&
				draft.serverKey.id.trim() !== "" &&
				draft.serverKey.secret !== "" &&
				draft.agentKey.id.trim() !== "" &&
				draft.agentKey.secret !== ""
			);
		case "local":
			return draft.path.trim() !== "";
		default:
			return draft.options.some((row) => row.key.trim() !== "");
	}
}

export function checkRows(
	result: CheckResult,
	backend: Backend,
	stale: boolean,
): CheckRow[] {
	const rows = toCheckRows(result, backend);
	return CHECK_IDS.map((id) => {
		const row = rows.find((r) => r.id === id);
		if (!row) return { id, status: "neutral" as const };
		return stale ? { ...row, status: "stale" as const } : row;
	});
}

export function runningRows(): CheckRow[] {
	return CHECK_IDS.map((id) => ({ id, status: "running" as const }));
}

export function lastGroup(recoveryKey: string) {
	return recoveryKey.split("-").at(-1) ?? "";
}

export type CheckState =
	| { phase: "idle" }
	| { phase: "running" }
	| { phase: "error"; message: string }
	| { phase: "done"; result: CheckResult; stale: boolean };
