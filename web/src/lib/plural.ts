const count = new Intl.NumberFormat("en", { maximumFractionDigits: 0 });
const noun = (n: number, one: string, many: string) => (n === 1 ? one : many);

export const onlineAgents = (n: number) =>
	`${count.format(n)} online ${noun(n, "agent", "agents")}`;

export const offlineAgents = (n: number) =>
	`${count.format(n)} offline ${noun(n, "agent", "agents")}`;

export const expiresInDays = (n: number) =>
	`Expires in ${count.format(n)} ${noun(n, "day", "days")}`;

export const hostCount = (n: number) =>
	`${count.format(n)} ${noun(n, "host", "hosts")}`;

export const jobCount = (n: number) =>
	`${count.format(n)} ${noun(n, "job", "jobs")}`;

export const policyCount = (n: number) =>
	`${count.format(n)} ${noun(n, "policy", "policies")}`;

export const warningCount = (n: number) =>
	`${count.format(n)} ${noun(n, "warning", "warnings")}`;

export const fileCount = (n: number) =>
	`${count.format(n)} ${noun(n, "file", "files")}`;

export const snapshotCount = (n: number) =>
	`${count.format(n)} ${noun(n, "snapshot", "snapshots")}`;

export const itemCount = (n: number) =>
	`${count.format(n)} ${noun(n, "item", "items")}`;

export const matchCount = (n: number) =>
	`${count.format(n)} ${noun(n, "match", "matches")}`;

export const errorCount = (n: number) =>
	`${count.format(n)} ${noun(n, "error", "errors")}`;

export const tokenCount = (n: number) =>
	`${count.format(n)} ${noun(n, "token", "tokens")}`;

export const scopeCount = (n: number) =>
	`${count.format(n)} ${noun(n, "scope", "scopes")}`;

export const dayCount = (n: number) =>
	`${count.format(n)} ${noun(n, "day", "days")}`;

export const packCount = (n: number) =>
	`${count.format(n)} ${noun(n, "pack", "packs")}`;

export const alertCount = (n: number) =>
	`${count.format(n)} ${noun(n, "alert", "alerts")}`;

export const problemCount = (n: number) =>
	`${count.format(n)} ${noun(n, "problem", "problems")}`;

export const channelCount = (n: number) =>
	`${count.format(n)} ${noun(n, "channel", "channels")}`;

export const repositoryCount = (n: number) =>
	`${count.format(n)} ${noun(n, "repository", "repositories")}`;

export const markerCount = (n: number) =>
	`${count.format(n)} ${noun(n, "marker", "markers")}`;
