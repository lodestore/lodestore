import { sourceCount } from "@/lib/plural";

export const sources = {
	type: {
		files: "Files",
		proxmoxVm: "Virtual machines",
		proxmoxCt: "Containers",
		postgres: "PostgreSQL",
		postgresWal: "PostgreSQL WAL",
		mysql: "MySQL",
	},
	noun: {
		proxmoxVm: "virtual machines",
		proxmoxCt: "containers",
		postgres: "PostgreSQL databases",
		mysql: "MySQL schemas",
	},
	host: {
		columns: {
			source: "Source",
			poolTags: "Pool · tags",
			details: "Details",
			protection: "Protection",
			lastSnapshot: "Last snapshot",
		},
		filter: {
			label: "Show",
			all: "All",
			unprotected: "Not protected",
			selectValue: (label: string, n: number) => `Show: ${label} (${n})`,
		},
		found: (when: string) => `Found by the agent ${when}`,
		foundWithScope: (scope: string, when: string) =>
			`${scope} · found by the agent ${when}`,
		scope: {
			cluster: (name: string) => `Proxmox cluster ${name}`,
			node: (name: string) => `Proxmox node ${name}`,
		},
		protection: {
			notProtected: "Not protected",
			agentTooOld: "Agent too old",
		},
		never: "Never",
		more: (n: number) => `+${n}`,
		showing: (from: number, to: number, total: number) =>
			total === 1
				? `Showing 1 of ${sourceCount(1)}`
				: `Showing ${from}–${to} of ${sourceCount(total)}`,
		agentTooOldNote: (nouns: string) =>
			`Agent too old: this agent version finds ${nouns} but can't back them up yet. Update the agent.`,
		truncated: (n: number) => `The agent reported the first ${n} sources only.`,
		clone: {
			cluster: (label: string, hosts: string) =>
				`${label} is the same cluster as on ${hosts}. Add --host to lode latest@ for this source.`,
			source: (label: string, hosts: string) =>
				`${label} is the same source as on ${hosts}. Add --host to lode latest@ for this source.`,
			more: (n: number) =>
				n === 1
					? "and 1 more source shares its key with other hosts."
					: `and ${n} more sources share their key with other hosts.`,
		},
		unreachable: {
			postgres:
				"The agent can't connect as the postgres user, so the databases are unknown.",
			mysql:
				"The agent can't connect as the root user, so the schemas are unknown.",
		},
		group: (name: string, version: string | undefined, port: number) =>
			[
				version ? `${name} ${version}` : name,
				port > 0 ? `port ${port}` : undefined,
			]
				.filter(Boolean)
				.join(" · "),
		oid: (n: number) => `OID ${n}`,
		filesLabel: "Files",
		empty: {
			heading: "The agent found no sources on this host.",
			body: "It checks again every few minutes.",
		},
		loadFailed:
			"Couldn't load the sources. Check that the server is reachable, then retry.",
		retry: "Retry loading sources",
	},
};
