const noun = (n: number, one: string, many: string) =>
	`${n} ${n === 1 ? one : many}`;

const agents = (n: number) => noun(n, "agent", "agents");

function timeout(secs: number) {
	if (secs >= 60 && secs % 60 === 0) {
		return noun(secs / 60, "minute", "minutes");
	}
	return noun(secs, "second", "seconds");
}

export const agentUpdate = {
	outdated: "Outdated",
	filter: (n: number) => `Outdated · ${n}`,
	banner: {
		head: (n: number) =>
			n === 1
				? "1 agent runs an older version."
				: `${n} agents run an older version.`,
		body: (version: string) =>
			`This server ships agent ${version}, signed by the project key.`,
	},
	updateOutdated: "Update outdated agents",
	unsigned: {
		head: "Agents can't be updated from this server.",
		body: (dir: string) =>
			`The agent binary in ${dir} has no .minisig signature. Use an official image or sign your build.`,
	},
	selected: (n: number) => `${n} selected`,
	updateSelected: "Update selected",
	clear: "Clear",
	updateOne: "Update agent",
	manual: "This agent can't update itself. Run install.sh on the host.",
	noBinary: {
		head: (version: string, arch: string) =>
			`This server has no signed agent ${version} for ${arch}.`,
		body: (arch: string, dir: string) =>
			`Add lodestore-agent-linux-${arch} and its .minisig to ${dir}.`,
	},
	selectHost: (host: string) => `Select ${host}`,
	selectAll: "Select all outdated hosts",
	state: {
		queued: "Queued",
		waiting_for_job: "Waiting for job",
		offline_queued: "Offline, queued",
		updating: (version: string) => `Updating to ${version}`,
		rolled_back: "Update rolled back",
	},
	confirm: {
		title: (n: number, version: string) => `Update ${agents(n)} to ${version}?`,
		titleOne: (host: string, version: string) =>
			`Update ${host} to ${version}?`,
		first: "First",
		pauseAfter: "Pause after",
		busy: "Busy hosts",
		offline: "Offline hosts",
		firstValue: (canary: number, wave: number) =>
			`${noun(canary, "canary host", "canary hosts")}, then waves of ${wave}`,
		pauseAfterValue: (threshold: number) =>
			`More than ${noun(threshold, "rollback", "rollbacks")}`,
		busyValue: "Update after their running backup or restore",
		offlineValue: (k: number) => `${k} · update when they reconnect`,
		note: (secs: number) =>
			`Each host checks the signature and hash, switches, and goes back to its current version if the new one does not connect within ${timeout(secs)}.`,
		submit: (n: number) => `Update ${agents(n)}`,
		submitOne: "Update agent",
		cancel: "Cancel",
	},
	card: {
		title: "Agent update to",
		running: (done: number, total: number) =>
			`Updating · ${done} of ${noun(total, "host", "hosts")}`,
		paused: (rolledBack: number, threshold: number) =>
			`Paused · ${noun(rolledBack, "host", "hosts")} rolled back, the limit is ${threshold}`,
		pausedBecause: (reason: string) => `Paused · ${reason}`,
		pausedManual: "Paused",
		done: (n: number) => `Done · ${n} updated`,
		doneRollbacks: (n: number, rolledBack: number) =>
			`Done · ${n} updated, ${rolledBack} rolled back`,
		cancelled: (n: number) => `Cancelled · ${n} updated`,
	},
	pause: "Pause",
	resume: "Resume",
	cancel: "Cancel update",
	dismiss: "Dismiss",
	cancelTitle: "Cancel the update?",
	cancelConfirm: (version: string) =>
		`Hosts already updated keep ${version}. Queued hosts stay on their version.`,
	keep: "Keep updating",
	steps: {
		canary: "Canary",
		wave: (n: number) => `Wave ${n}`,
		more: (n: number) => `${n} more waves`,
		canaryDone: (updated: number, total: number) =>
			`${updated} of ${total} updated`,
		waveDone: (updated: number, rolledBack: number, waiting: number) =>
			`${updated} updated, ${rolledBack} rolled back, ${waiting} waiting`,
		queued: (n: number) => `${noun(n, "host", "hosts")} queued`,
		skipped: (n: number) => `${noun(n, "host", "hosts")} skipped`,
	},
	jobs: "Open update jobs",
	loadFailed: "Couldn't load the agent update status.",
	conflict: "An update is already running.",
} as const;
