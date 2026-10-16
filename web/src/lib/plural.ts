const noun = (n: number, one: string, many: string) => (n === 1 ? one : many);

export const onlineAgents = (n: number) =>
	`${n} online ${noun(n, "agent", "agents")}`;

export const offlineAgents = (n: number) =>
	`${n} offline ${noun(n, "agent", "agents")}`;

export const expiresInDays = (n: number) =>
	`Expires in ${n} ${noun(n, "day", "days")}`;

export const hostCount = (n: number) => `${n} ${noun(n, "host", "hosts")}`;

export const jobCount = (n: number) => `${n} ${noun(n, "job", "jobs")}`;

export const policyCount = (n: number) =>
	`${n} ${noun(n, "policy", "policies")}`;

export const warningCount = (n: number) =>
	`${n} ${noun(n, "warning", "warnings")}`;

export const fileCount = (n: number) => `${n} ${noun(n, "file", "files")}`;
