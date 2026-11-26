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

export const BACKENDS = ["s3", "gcs", "sftp", "azblob", "local"] as const;
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
export type AgentSecrets = {
	keyId: string;
	secret: string;
	credential: string;
	user: string;
	privateKey: string;
	sas: string;
};

export const DEFAULT_AGENT_USER = "lodestore-agent";
export const CHECK_TIMEOUT_MS = 60_000;
export type AzureCredentialKind = "account_key" | "sas";

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
	serverCredential: string;
	serverUser: string;
	serverPrivateKey: string;
	hostKey: string;
	container: string;
	accountName: string;
	azureCredentialKind: AzureCredentialKind;
	agent: AgentSecrets;
	skipRights: boolean;
	acceptWeakerGuarantee: boolean;
};

const emptyKey = (): KeyPair => ({ id: "", secret: "" });
export const emptyAgent = (): AgentSecrets => ({
	keyId: "",
	secret: "",
	credential: "",
	user: "",
	privateKey: "",
	sas: "",
});

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
		serverCredential: "",
		serverUser: "",
		serverPrivateKey: "",
		hostKey: "",
		container: "",
		accountName: "",
		azureCredentialKind: "account_key",
		agent: emptyAgent(),
		skipRights: false,
		acceptWeakerGuarantee: false,
	};
}

export function withoutSecrets(draft: Draft): Draft {
	return {
		...draft,
		serverKey: emptyKey(),
		serverCredential: "",
		serverPrivateKey: "",
		hostKey: "",
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

export function presetUsesServerKey(preset: PresetInfo | null) {
	return preset?.backend === "s3" && preset.mechanism === "instruction";
}

export function agentUsesServerKey(draft: Draft) {
	return presetUsesServerKey(draft.preset);
}

export function azureVendsSas(draft: Draft) {
	return (
		draft.preset?.backend === "azblob" &&
		draft.azureCredentialKind === "account_key"
	);
}

export function agentCanSkip(draft: Draft) {
	return !agentUsesServerKey(draft) && !azureVendsSas(draft);
}

export function agentUserOf(preset: PresetInfo | null, agent: AgentSecrets) {
	const fallback =
		preset?.mechanism === "instruction" ? "" : DEFAULT_AGENT_USER;
	return agent.user.trim() || fallback;
}

export function serverUserOf(draft: Draft) {
	return draft.preset?.endpoint_template
		? placeholderValue(draft, "user")
		: draft.serverUser.trim();
}

export function hostKeyScope(draft: Draft) {
	return { endpoint: endpointOf(draft), root: draft.prefix.trim() };
}

export function withFreshHostKey(before: Draft, after: Draft): Draft {
	if (after.hostKey === "") return after;
	const was = hostKeyScope(before);
	const now = hostKeyScope(after);
	return was.endpoint === now.endpoint && was.root === now.root
		? after
		: { ...after, hostKey: "" };
}

export function toAgentPayload(
	backend: Backend,
	agent: AgentSecrets,
): Pick<RepositoryDraft, "agent_credentials" | "agent_options"> {
	switch (backend) {
		case "s3":
			return {
				agent_credentials: {
					access_key_id: agent.keyId.trim(),
					secret_access_key: agent.secret,
				},
			};
		case "gcs":
			return { agent_options: { credential: agent.credential } };
		case "sftp":
			return {
				agent_options: { user: agent.user.trim(), key: agent.privateKey },
			};
		case "azblob":
			return agent.sas.trim() === ""
				? {}
				: { agent_options: { sas_token: agent.sas.trim() } };
		default:
			return {};
	}
}

function compact(entries: Record<string, string>) {
	return Object.fromEntries(
		Object.entries(entries)
			.map(([key, value]) => [key, value.trim()] as const)
			.filter(([, value]) => value !== ""),
	);
}

export function toApiDraft(draft: Draft): RepositoryDraft {
	const accept = draft.acceptWeakerGuarantee
		? { accept_weaker_guarantee: true }
		: {};
	const { preset } = draft;
	const base = { name: draft.name, ...accept };
	const limited = (backend: Backend, agent: AgentSecrets = draft.agent) =>
		draft.skipRights ? {} : toAgentPayload(backend, agent);
	switch (preset?.backend) {
		case "s3": {
			const options = compact({
				bucket: draft.bucket,
				region: draft.region,
				endpoint: endpointOf(draft),
				prefix: draft.prefix,
			});
			if (draft.pathStyle !== null)
				options.path_style = String(draft.pathStyle);
			if (preset.provider) options.provider = preset.provider;
			const server = {
				access_key_id: draft.serverKey.id.trim(),
				secret_access_key: draft.serverKey.secret,
			};
			const shared = draft.skipRights || agentUsesServerKey(draft);
			return {
				...base,
				location: { backend: "s3", options },
				server_credentials: server,
				...(shared
					? { agent_credentials: server }
					: toAgentPayload("s3", draft.agent)),
			};
		}
		case "gcs":
			return {
				...base,
				location: {
					backend: "gcs",
					options: compact({
						bucket: draft.bucket,
						root: draft.prefix,
						credential: draft.serverCredential,
					}),
				},
				...limited("gcs"),
			};
		case "sftp":
			return {
				...base,
				location: {
					backend: "sftp",
					options: {
						...compact({
							endpoint: endpointOf(draft),
							user: serverUserOf(draft),
							root: draft.prefix,
							host_key: draft.hostKey,
						}),
						...(draft.serverPrivateKey ? { key: draft.serverPrivateKey } : {}),
					},
				},
				...limited("sftp", {
					...draft.agent,
					user: agentUserOf(preset, draft.agent),
				}),
			};
		case "azblob": {
			const sas = draft.azureCredentialKind === "sas";
			return {
				...base,
				location: {
					backend: "azblob",
					options: compact({
						account_name: draft.accountName,
						container: draft.container,
						root: draft.prefix,
						endpoint: endpointOf(draft),
						[sas ? "sas_token" : "account_key"]: draft.serverCredential,
					}),
				},
				...(sas ? limited("azblob") : {}),
			};
		}
		default:
			return {
				...base,
				location: { backend: "local", options: compact({ path: draft.path }) },
			};
	}
}

function endpointReady(draft: Draft) {
	return (
		endpointOf(draft) !== "" &&
		placeholdersOf(draft.preset?.endpoint_template).every(
			(name) => placeholderValue(draft, name) !== "",
		)
	);
}

export function connectionReady(draft: Draft) {
	const { preset } = draft;
	if (!preset) return false;
	switch (preset.backend) {
		case "local":
			return draft.path.trim() !== "";
		case "gcs":
			return draft.bucket.trim() !== "" && draft.serverCredential !== "";
		case "azblob":
			return (
				draft.accountName.trim() !== "" &&
				draft.container.trim() !== "" &&
				draft.serverCredential !== ""
			);
		case "sftp":
			return (
				endpointReady(draft) &&
				serverUserOf(draft) !== "" &&
				draft.serverPrivateKey !== "" &&
				draft.prefix.trim() !== "" &&
				draft.hostKey !== ""
			);
		default:
			return (
				draft.bucket.trim() !== "" &&
				draft.serverKey.id.trim() !== "" &&
				draft.serverKey.secret !== "" &&
				placeholdersOf(preset.endpoint_template).every(
					(name) => placeholderValue(draft, name) !== "",
				)
			);
	}
}

export function agentFilled(preset: PresetInfo | null, agent: AgentSecrets) {
	switch (preset?.backend) {
		case "gcs":
			return agent.credential !== "";
		case "sftp":
			return agentUserOf(preset, agent) !== "" && agent.privateKey !== "";
		case "azblob":
			return agent.sas.trim() !== "";
		default:
			return agent.keyId.trim() !== "" && agent.secret !== "";
	}
}

export function rightsReady(draft: Draft) {
	if (draft.preset?.backend === "local") return true;
	if (azureVendsSas(draft)) return true;
	return (
		draft.skipRights ||
		agentUsesServerKey(draft) ||
		agentFilled(draft.preset, draft.agent)
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
