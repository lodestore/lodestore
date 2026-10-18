export const hostBackup = {
	runBackup: "Run backup",
	startFailed: "start the backup",
	allPolicies: (n: number) => `All policies (${n})`,
	policiesCard: "Policies",
	noPolicy:
		"No policy applies to this host. Add a label that a policy selects, or create a policy.",
	createPolicy: "Create policy",
	noPolicyReason: "No policy applies to this host.",
	jobsEmptyHeading: "No jobs on this host yet",
	jobsEmptyBody: "Run a policy on this host and its jobs appear here.",
	openInJobs: "Open in Jobs",
} as const;

export const snapshots = {
	title: "Snapshots",
	emptyHeading: "No snapshots yet",
	emptyBody: "Once a job finishes, its snapshot is listed here.",
	footnote: "Open a snapshot in Restore to browse its files.",
	loadFailed:
		"Couldn't load snapshots. Check that the server is reachable, then retry.",
	columns: {
		created: "Created",
		snapshot: "Snapshot",
		source: "Source",
		contents: "Contents",
		size: "Size",
		files: "Files",
		status: "Status",
		actions: "Actions",
	},
	browseFiles: "Browse files",
	browseFilesFor: (id: string) => `Browse files in snapshot ${id}`,
	cliSource: (name: string) => `CLI · ${name}`,
	incomplete: "Incomplete",
	complete: "Complete",
	copyId: "Copy snapshot ID",
	syncCatalog: "Check storage for new snapshots",
	syncing: "Checking storage…",
	catalogUpdated: (n: number) =>
		`Catalog updated. ${n} new ${n === 1 ? "snapshot" : "snapshots"}.`,
	syncFailed: "update the catalog",
	suspicious: "Suspicious",
	policyDeleted: "Policy deleted",
	policyDeletedInline: "policy deleted",
	policyDeletedTip:
		"The policy was deleted. Retention no longer applies to this snapshot. Delete it by hand when you no longer need it.",
	suspiciousLabel: (reasons: string) => `Suspicious. ${reasons}`,
	markedBy: (user: string, reason: string) => `Marked by ${user}: ${reason}`,
	reasons: {
		files_drop: (pct: string, from: string, to: string) =>
			`${pct}% fewer files than before (${from} to ${to}).`,
		bytes_drop: (pct: string, from: string, to: string) =>
			`${pct}% less data than before (${from} to ${to}).`,
		changed_spike: (pct: string, factor: string) =>
			`${pct}% of files changed, about ${factor} times the usual.`,
		added_spike: (size: string, factor: string) =>
			`${size} of new data, about ${factor} times the usual.`,
		unknown: "Unusual change compared with earlier snapshots.",
	},
	menu: {
		actionsFor: (id: string) => `Actions for snapshot ${id}`,
		mark: "Mark as suspicious",
		clear: "Clear suspicious mark",
		delete: "Delete snapshot",
	},
	reason: {
		label: "Reason",
		required: "Enter a reason.",
	},
	markDialog: {
		title: (id: string) => `Mark snapshot ${id} as suspicious`,
		body: "A suspicious snapshot is not counted by retention and is never forgotten automatically. You can clear the mark later.",
		placeholder: "Why do you think it is suspicious?",
		confirm: "Mark as suspicious",
		cancel: "Don't mark",
		action: "mark the snapshot as suspicious",
	},
	clearDialog: {
		title: (id: string) => `Clear the suspicious mark on ${id}?`,
		body: "The snapshot returns to normal retention. If your rules no longer keep it, the next backup of this host can forget it.",
		placeholder: "Why is this snapshot fine?",
		confirm: "Clear mark",
		cancel: "Keep mark",
		action: "clear the mark",
	},
	deleteDialog: {
		title: (id: string) => `Delete snapshot ${id}?`,
		body: "It is removed from the repository, from Restore and from lode. This cannot be undone. Storage space is freed later by garbage collection.",
		confirm: "Delete snapshot",
		cancel: "Keep snapshot",
		action: "delete the snapshot",
	},
	toast: {
		marked: (id: string) => `Snapshot ${id} marked as suspicious.`,
		cleared: (id: string) => `Suspicious mark cleared on snapshot ${id}.`,
		deleted: (id: string) => `Snapshot ${id} deleted.`,
		kept: (id: string) =>
			`Snapshot ${id} was removed from the catalog, but storage kept the file because of Object Lock. It stays out of Restore.`,
	},
} as const;
