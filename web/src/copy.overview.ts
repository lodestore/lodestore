import type { HostState } from "@/api/gen/model";
import { alertCount, hostCount, problemCount } from "@/lib/plural";

const plural = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"}`;

export function longDuration(seconds: number) {
	const days = Math.floor(seconds / 86_400);
	const hours = Math.floor((seconds % 86_400) / 3_600);
	const minutes = Math.floor((seconds % 3_600) / 60);
	const parts = [
		days > 0 ? plural(days, "day") : "",
		hours > 0 ? plural(hours, "hour") : "",
		minutes > 0 ? plural(minutes, "minute") : "",
	].filter(Boolean);
	return parts.length > 0 ? parts.join(" ") : "1 minute";
}

const sentenceEnd = (text: string) => text.replace(/\.+$/, "");

export const overview = {
	title: "Overview",

	staleData: ["Showing data from ", "."],
	loadFailed:
		"Couldn't load the fleet overview. Check that the server is reachable, then retry.",
	retry: "Retry loading",
	empty: {
		heading: "No hosts yet",
		body: "Add a host to start backing it up. The fleet overview appears here once a host reports in.",
		action: "Add host",
	},
	status: {
		overdue: "Overdue",
		silenced: "Silenced",
		passed: "Passed",
		protected: "Protected",
		notProtected: "Not protected",
		neverRun: "Never run",
	},
	states: {
		failed: "Failed",
		overdue: "Overdue",
		offline: "Offline",
		notProtected: "Not protected",
		healthy: "Protected",
	},
	verdict: {
		attention: (k: number, n: number) =>
			k === 1
				? `${k} of ${hostCount(n)} needs attention`
				: `${k} of ${hostCount(n)} need attention`,
		allProtected: (n: number) =>
			n === 1 ? "Your host is protected" : `All ${n} hosts protected`,
		newest: ["Newest backup ", "."],
	},
	legend: { label: "Filter hosts by state", help: "What the states mean" },
	storage: {
		stored: (size: string) => `Stored ${size}`,
		dedup: (ratio: string) => `dedup ${ratio}`,
		sep: " · ",
		counted: "counted ",
		notCounted: "Storage used is calculated after the first structure check.",
	},
	filter: { showing: (label: string) => `Showing: ${label}` },
	sidebar: {
		label: (n: number) => `${n} active ${n === 1 ? "alert" : "alerts"}`,
		badge: (n: number) => (n > 99 ? "99+" : String(n)),
	},
	stateHelp: {
		failed: "The last run of a policy on this host failed.",
		overdue: "A policy has no successful backup within its time limit.",
		offline:
			"The agent has been out of touch longer than the offline threshold.",
		not_protected: "No policy with a time limit has backed up this host yet.",
		healthy: "Backups are within their time limits and nothing has failed.",
	} satisfies Record<HostState, string>,
	table: {
		heading: "Hosts",
		columns: {
			host: "Host",
			status: "Status",
			lastBackup: "Last success",
			size: "Size",
			added: "Added",
			agent: "Agent",
			actions: "Actions",
		},
		actionsFor: (host: string) => `Actions for ${host}`,
		viewHost: "View host",
		runBackup: "Run backup",
		never: "Never",
		agentOnline: "Online",
		agentOffline: "Offline",
		filterEmpty: "No hosts match this filter.",
		clearFilter: "Clear filter",
		groups: {
			attention: "Needs attention",
			protected: "Protected",
			header: (label: string, n: number) => `${label} · ${n}`,
		},
		reason: {
			offline: (relative?: string) =>
				relative
					? `Agent last seen ${relative}`
					: "Agent has not connected yet",
			noSuccessYet: "No scheduled backup has succeeded yet",
			noPolicy: "No backup policy applies to this host",
			separator: " · ",
		},
		fix: {
			policy: "Open policy",
			agent: "Check agent",
			add: "Add policy",
			host: "Open host",
		},
		fixFor: {
			policy: (policy: string, host: string) =>
				`Open policy ${policy} for ${host}`,
			agent: (host: string) => `Check agent on ${host}`,
			add: (host: string) => `Add policy for ${host}`,
			host: (host: string) => `Open host ${host}`,
		},
		moreLabels: (n: number) => `+${n}`,
		statusOverdue: (policy: string, age: number, limit?: number | null) =>
			`${policy} · no success for ${longDuration(age)}${
				limit == null ? "" : `, limit ${longDuration(limit)}`
			}`,
		statusFailed: (policy: string, error?: string | null) =>
			`${policy} · ${error ? sentenceEnd(error) : "backup failed"}`,
	},
	alerts: {
		otherTitle: "Repository and server alerts",
		partial: (shown: number, total: number) =>
			`Showing ${shown} of ${total} active alerts. Use the API to list all alerts.`,
		showAll: (n: number) => `Show all ${n} alerts`,
		showFewer: "Show fewer alerts",
		unknownHost: "unknown host",
		unknownPolicy: "unknown",
		unknownRepository: "unknown repository",
		more: (n: number) => `and ${n} more. Use the API to list all alerts.`,
		since: "Since ",
		loadFailed:
			"Couldn't load active alerts. Check that the server is reachable, then retry.",
		newAlerts: (n: number) => (n === 1 ? "1 new alert." : `${n} new alerts.`),
		count: (n: number) => alertCount(n),
		titles: {
			backupFailed: (host: string) => `Backup failed on ${host}`,
			noRecent: (host: string) => `No recent backup on ${host}`,
			offline: (host: string) => `${host} is offline`,
			agentUpdateFailed: (host: string) => `Agent update failed on ${host}`,
			checkFailed: (level: string, repository: string) =>
				`${level} check failed on ${repository}`,
			snapshotQuarantined: (count: number, repository: string) =>
				`${plural(count, "snapshot")} in ${repository} cannot be read; cleanup is stopped`,
			repositoryUnavailable: (repository: string) =>
				`Repository ${repository} is unavailable`,
			expiresIn: (name: string, days: number) =>
				`${name} expires in ${plural(days, "day")}`,
			expired: (name: string) => `${name} has expired`,
		},
		certificates: {
			agent: (host: string) => `Agent certificate on ${host}`,
			grpc: "Server gRPC certificate",
		},
		levels: {
			structure: "Structure",
			packs: "Pack reading",
			restore: "Test restore",
		} as Record<string, string>,
		details: {
			backupFailed: (policy: string, error?: string | null) =>
				`Policy ${policy}: ${error ? sentenceEnd(error) : "The backup failed"}.`,
			noRecent: (relative?: string, limit?: number | null) =>
				`${relative ? `Last success ${relative}.` : "No successful backup yet."}${
					limit == null
						? ""
						: ` Expected at least every ${longDuration(limit)}.`
				}`,
			offline: (relative?: string) =>
				relative
					? `Last contact ${relative}.`
					: "The agent has not connected yet.",
			checkFailed: (n: number) =>
				`${problemCount(n)} found. Open the report in Integrity checks.`,
			agentUpdateFailed: (error?: string | null) =>
				`${error ? sentenceEnd(error) : "The update was rolled back"}.`,
		},
		silence: {
			trigger: "Silence alerts",
			triggerFor: (host: string) => `Silence alerts for ${host}`,
			resume: "Resume alerts",
			durations: {
				hour: "For 1 hour",
				day: "For 1 day",
				week: "For 1 week",
				forever: "Until I remove it",
			},
			badge: (until: string) => `Silenced · until ${until}`,
			badgeForever: "Silenced · until removed",
			silenced: (host: string, duration: string) =>
				duration === "forever"
					? `Alerts for ${host} are silenced until you remove it.`
					: `Alerts for ${host} are silenced for 1 ${duration}.`,
			resumed: (host: string) => `Alerts for ${host} resumed.`,
			failed: (host: string) => `silence alerts for ${host}`,
			resumeFailed: (host: string) => `resume alerts for ${host}`,
		},
	},
};
