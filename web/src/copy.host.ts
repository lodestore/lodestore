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
	footnote: "Restore arrives in a later release.",
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
	},
	cliSource: (name: string) => `CLI · ${name}`,
	incomplete: "Incomplete",
	complete: "Complete",
	copyId: "Copy snapshot ID",
	syncCatalog: "Check storage for new snapshots",
	syncing: "Checking storage…",
	catalogUpdated: (n: number) =>
		`Catalog updated. ${n} new ${n === 1 ? "snapshot" : "snapshots"}.`,
	syncFailed: "update the catalog",
} as const;
