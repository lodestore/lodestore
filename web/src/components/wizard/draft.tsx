import type {
	CheckResult,
	RepositoryDraft,
	StoragePresetDto,
} from "@/api/gen/model";
import {
	CHECK_IDS,
	type CheckRow,
	toCheckRows,
} from "@/components/capability-checklist";
import { UNCHECKED_LEVEL, WEAK_LEVEL } from "@/components/guarantee-notes";
import { copy } from "@/copy";

export const STEPS = [
	"storage",
	"connection",
	"rights",
	"key",
	"host",
] as const;
export type Step = (typeof STEPS)[number];

export function parseStep(value: unknown): Step | undefined {
	return STEPS.find((step) => step === value);
}

export const BACKENDS = ["s3", "local"] as const;
export type Backend = (typeof BACKENDS)[number];

export type PresetInfo = Pick<
	StoragePresetDto,
	| "id"
	| "label"
	| "backend"
	| "endpoint_template"
	| "region"
	| "path_style"
	| "provider"
	| "mechanism"
>;

export const LOCAL_PRESET: PresetInfo = {
	id: "local",
	label: copy.presets.local,
	backend: "local",
	endpoint_template: null,
	region: null,
	path_style: false,
	provider: null,
	mechanism: "none",
};

export function backendOf(preset: PresetInfo | null): Backend | undefined {
	return BACKENDS.find((b) => b === preset?.backend);
}

export type KeyPair = { id: string; secret: string };
export type AgentSecrets = { keyId: string; secret: string };

export type Draft = {
	name: string;
	preset: PresetInfo | null;
	endpoint: string;
	region: string;
	bucket: string;
	prefix: string;
	pathStyle: boolean | null;
	path: string;
	templateValues: Record<string, string>;
	serverKey: KeyPair;
	agent: AgentSecrets;
	skipRights: boolean;
	acceptWeakerGuarantee: boolean;
};

const emptyKey = (): KeyPair => ({ id: "", secret: "" });
const emptyAgent = (): AgentSecrets => ({ keyId: "", secret: "" });

export const DEFAULT_NAME = "default";
export const NAME_PATTERN = /^[a-z0-9-]{1,48}$/;

export function newDraft(): Draft {
	return {
		name: DEFAULT_NAME,
		preset: null,
		endpoint: "",
		region: "",
		bucket: "",
		prefix: "",
		pathStyle: null,
		path: "",
		templateValues: {},
		serverKey: emptyKey(),
		agent: emptyAgent(),
		skipRights: false,
		acceptWeakerGuarantee: false,
	};
}

export function withoutSecrets(draft: Draft): Draft {
	return {
		...draft,
		serverKey: emptyKey(),
		agent: emptyAgent(),
	};
}

export function choosePreset(draft: Draft, preset: PresetInfo): Draft {
	return {
		...newDraft(),
		name: draft.name,
		preset,
		pathStyle: preset.path_style,
		region: preset.region ?? "",
	};
}

export function placeholdersOf(template: string | null | undefined) {
	return [...(template ?? "").matchAll(/\{(\w+)\}/g)].map((m) => m[1]);
}

function placeholderValue(draft: Draft, name: string) {
	return (
		name === "region" ? draft.region : (draft.templateValues[name] ?? "")
	).trim();
}

export function endpointOf(draft: Draft) {
	const template = draft.preset?.endpoint_template;
	if (!template) return draft.endpoint.trim();
	return template.replace(
		/\{(\w+)\}/g,
		(token, name: string) => placeholderValue(draft, name) || token,
	);
}

export function agentUsesServerKey(draft: Draft) {
	return (
		draft.preset?.backend === "s3" && draft.preset.mechanism === "instruction"
	);
}

export function toAgentPayload(
	backend: Backend,
	agent: AgentSecrets,
): Pick<RepositoryDraft, "agent_credentials"> {
	if (backend !== "s3") return {};
	return {
		agent_credentials: {
			access_key_id: agent.keyId.trim(),
			secret_access_key: agent.secret,
		},
	};
}

export function toApiDraft(draft: Draft): RepositoryDraft {
	const accept = draft.acceptWeakerGuarantee
		? { accept_weaker_guarantee: true }
		: {};
	const options: Record<string, string> = {};
	const put = (key: string, value: string) => {
		const trimmed = value.trim();
		if (trimmed !== "") options[key] = trimmed;
	};
	const { preset } = draft;
	if (preset?.backend === "s3") {
		put("bucket", draft.bucket);
		put("region", draft.region);
		put("endpoint", endpointOf(draft));
		put("prefix", draft.prefix);
		if (draft.pathStyle !== null) options.path_style = String(draft.pathStyle);
		if (preset.provider) options.provider = preset.provider;
		const server = {
			access_key_id: draft.serverKey.id.trim(),
			secret_access_key: draft.serverKey.secret,
		};
		const shared = draft.skipRights || agentUsesServerKey(draft);
		return {
			name: draft.name,
			...accept,
			location: { backend: "s3", options },
			server_credentials: server,
			...(shared
				? { agent_credentials: server }
				: toAgentPayload("s3", draft.agent)),
		};
	}
	put("path", draft.path);
	return {
		name: draft.name,
		...accept,
		location: { backend: "local", options },
	};
}

export function connectionReady(draft: Draft) {
	const { preset } = draft;
	if (!preset) return false;
	if (preset.backend === "local") return draft.path.trim() !== "";
	return (
		draft.bucket.trim() !== "" &&
		draft.serverKey.id.trim() !== "" &&
		draft.serverKey.secret !== "" &&
		placeholdersOf(preset.endpoint_template).every(
			(name) => placeholderValue(draft, name) !== "",
		)
	);
}

export function rightsReady(draft: Draft) {
	if (draft.preset?.backend !== "s3") return true;
	return (
		draft.skipRights ||
		agentUsesServerKey(draft) ||
		(draft.agent.keyId.trim() !== "" && draft.agent.secret !== "")
	);
}

export function needsConsent(result: CheckResult) {
	return (
		result.guarantee_level === WEAK_LEVEL ||
		result.guarantee_level === UNCHECKED_LEVEL
	);
}

export function checkRows(
	result: CheckResult,
	backend: string,
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
