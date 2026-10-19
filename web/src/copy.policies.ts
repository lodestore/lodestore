export const policies = {
	title: "Policies",
	newTitle: "New policy",
	create: "Create policy",
	save: "Save policy",
	discard: "Discard changes",
	runNow: "Run now",
	edit: "Edit",
	delete: "Delete policy",
	emptyHeading: "No policies yet",
	emptyBody:
		"A policy chooses which hosts to back up by label and what to include. Create one, then run it on your hosts.",
	noRepoBody: "Policies back up into a repository. Add one first.",
	addRepository: "Add repository",
	loadFailed:
		"Couldn't load policies. Check that the server is reachable, then retry.",
	notFoundHeading: "Policy not found",
	notFoundBody: "It may have been removed, or the link is wrong.",
	backToPolicies: "Back to Policies",
	actionsFor: (name: string) => `Actions for ${name}`,
	openRow: (name: string) => `Open ${name}`,
	columns: {
		name: "Name",
		selector: "Selector",
		schedule: "Schedule",
		hosts: "Hosts",
		lastRun: "Last run",
		actions: "Actions",
	},
	allHosts: "All hosts",
	pickedHosts: "Selected hosts",
	neverRun: "Never run",
	manualOnly: "Manual only",
	next: "Next",
	scheduleHint: (summary: string, zone: string) => `${summary} · ${zone}`,
	otherRepos: (n: number) => `+${n} in other repositories`,
	moreLabels: (n: number) => `+${n}`,
	runNowDisabled: "No hosts match this policy yet.",
	subtitle: (repo: string, hosts: string) => `Repository ${repo} · ${hosts}`,
	moreActions: "More actions",
	panes: {
		form: "Form",
		yaml: "YAML",
		formAria: "Policy form",
		yamlAria: "Policy YAML",
	},
	sync: {
		inSync: "In sync",
		updating: "Updating…",
		failed: "Not synced",
	},
	footer: {
		unsaved: "Unsaved changes",
		saved: "All changes saved",
		more: "More editor actions",
	},
	editor: {
		tabForm: "Form",
		tabYaml: "YAML",
		yamlLabel: "Policy YAML",
		yamlNote: "Editing in the form rewrites the YAML and removes comments.",
		yamlBlocked: "Fix the YAML errors before switching to the form.",
		yamlInvalid:
			"The YAML has errors. The form shows the last valid version and is read-only until you fix them.",
		yamlStale:
			"Showing the last valid YAML. Fix the errors in the form to update it.",
		changedElsewhere:
			"This policy was changed elsewhere. Reload to see the saved version, then apply your edits again. Saving is paused until you reload.",
		reload: "Reload",
		formBlocked: "Fix the errors in the form before switching to YAML.",
		cannotSave: (first: string) =>
			first
				? `Can't save yet: ${first}`
				: "Can't save yet: fix the errors above.",
		yamlLine: (n: number, message: string) => `Line ${n}: ${message}`,
		recentJobs: "Recent jobs",
		saved: (name: string) => `Policy "${name}" saved.`,
		savedManaged: (name: string, manager: string) =>
			`Policy "${name}" saved. The next apply from ${manager} overwrites it.`,
		saveFailed: (serverMessage?: string) =>
			`Couldn't save the policy. ${serverMessage ? `${serverMessage}. ` : ""}Try again.`,
	},
	form: {
		name: "Name",
		nameHelper:
			'Lowercase letters, digits and "-". The name cannot be changed later.',
		nameLocked: "The name cannot be changed.",
		namePlaceholder: "etc-and-home",
		repository: "Repository",
		repositoryHelper: "Selected hosts are backed up into this repository.",
		repositoryPlaceholder: "Choose a repository",
		hosts: "Hosts",
		hostMode: "How to choose hosts",
		byLabels: "By labels",
		pickHosts: "Pick hosts",
		hostsHelper:
			"A host is backed up when it has all of these labels and uses this repository. New hosts with these labels are added automatically. Leave empty to include every host in the repository.",
		pickHostsHelper:
			"Only the hosts you pick are backed up. New hosts are not added automatically.",
		hostsLoading: "Loading hosts…",
		noHostsInRepo: "No hosts use this repository yet.",
		noHostsFound: "No hosts match the search.",
		searchHosts: "Search by host or label",
		pickedCount: (n: number, total: number) => `${n} of ${total} selected`,
		selectAll: "Select all",
		selectShown: "Select shown",
		showAllHosts: (n: number) => `Show all ${n} hosts`,
		clearHosts: "Clear",
		unknownHost: "Unknown or revoked host",
		otherRepoHost: (name: string) => `${name} (uses another repository)`,
		hostsFailed: "Couldn't load hosts.",
		pickAtLeastOne: "Pick at least one host.",
		paths: "Paths",
		pathsHelper:
			"Absolute paths on the host. If a path does not exist, the job fails.",
		pathPlaceholder: "/var/lib/app",
		addPath: "Add path",
		removePath: "Remove path",
		exclude: "Exclude",
		excludeHelper:
			"One pattern per line, gitignore syntax. Example: **/node_modules",
		options: "Options",
		oneFileSystem: "Stay on one filesystem",
		oneFileSystemHelper:
			"Don't descend into other mounted filesystems under these paths, such as network shares or /proc.",
		cacheDirTag: "Skip cache directories",
		cacheDirTagHelper:
			"Skips directories that contain a CACHEDIR.TAG file. Tools such as Cargo, ccache and pip mark caches this way; their contents can be rebuilt.",
		hooks: "Hooks (optional)",
		hooksHelper:
			"Hooks run on the host as root, one after another. Their output (stdout and stderr) goes to the job log.",
		before: "Before backup",
		beforeHelper:
			"Run in order, top to bottom. If one fails or times out, the rest are skipped and the backup does not start.",
		after: "After backup",
		afterHelper:
			"Run in order after the backup, whatever its result. A failure is logged as a warning and the snapshot is kept.",
		step: (n: number) => `${n}.`,
		hookType: "Hook type",
		script: "Script",
		scriptHelper:
			"A file name from /etc/lodestore/hooks.d on the host, owned by root.",
		scriptPlaceholder: "dump-db.sh",
		command: "Command",
		commandHelper:
			"Runs with /bin/sh -c. The host must allow it: hooks.allow_inline: true in /etc/lodestore/agent.yaml, otherwise the job fails.",
		commandPlaceholder: "pg_dump -U postgres app > /var/backups/app.sql",
		commandArgsHelper: "Passed to the command as $1, $2 and so on.",
		argsInYaml: "Empty or multi-line arguments can only be edited in YAML.",
		moveUp: (n: number) => `Move hook ${n} up`,
		moveDown: (n: number) => `Move hook ${n} down`,
		arguments: "Arguments (one per line)",
		timeout: "Timeout",
		timeoutHelper: "Default 1h. Examples: 90s, 30m, 2h.",
		timeoutPlaceholder: "1h",
		addBefore: "Add before-backup hook",
		addAfter: "Add after-backup hook",
		removeHook: "Remove hook",
	},
	schedule: {
		legend: "Schedule",
		cron: "Cron",
		cronPlaceholder: "30 2 * * *",
		cronHelper:
			"Five fields or @hourly, @daily, @weekly, @monthly. Leave empty to run backups only by hand.",
		timezone: "Time zone",
		timezoneSearch: "Search time zones",
		timezoneCurrent: (zone: string) => `Time zone: ${zone}`,
		timezoneNoMatch: (query: string) => `No time zone matches "${query}".`,
		options: "Options",
		optionsHeader: (jitter: string, window: string) =>
			`Options · jitter ${jitter} · catch up ${window}`,
		jitter: "Jitter",
		jitterPlaceholder: "15m",
		catchUp: "Catch up window",
		catchUpPlaceholder: "24h",
		optionsHelper:
			"Catch up runs one backup after missed slots, if the last missed slot is within the window.",
		optionsNeedCron: "Set a cron expression to use these options.",
		nextRuns: "Next runs",
		jitterNote: (jitter: string) => `Each host starts up to ${jitter} later.`,
		skipped: (date: string, time: string, actual: string) =>
			`${date}: ${time} does not exist that day, so the backup runs once at ${actual}.`,
		skippedNoRun: (date: string, time: string) =>
			`${date}: ${time} does not exist that day, so that run is skipped.`,
		repeated: (date: string, time: string) =>
			`${date}: ${time} happens twice that day, so the backup runs only the first time.`,
	},
	limits: {
		legend: "Limits",
		noLimits: "no limits",
		header: (parts: string[]) =>
			parts.length === 0
				? "Limits · no limits"
				: `Limits · ${parts.join(" · ")}`,
		upload: (rate: string) => `upload ${rate}`,
		maxRun: (value: string) => `max run ${value}`,
		nice: (n: string) => `nice ${n}`,
		io: (kind: string) => `I/O ${kind}`,
		ioKindBestEffort: "best effort",
		ioKindIdle: "idle",
		uploadRate: "Upload rate",
		uploadPlaceholder: "Unlimited",
		uploadHelper:
			"Caps the upload speed to storage. Examples: 20MiB/s, 500KiB/s.",
		maxRuntimeLabel: "Max run time",
		maxRuntimePlaceholder: "12h",
		maxRuntimeHelper: "A backup that runs longer stops and saves no snapshot.",
		niceLabel: "CPU priority (nice)",
		nicePlaceholder: "Default",
		niceHelper: "Higher means lower priority. 0 keeps the default.",
		ioPriority: "I/O priority",
		ioDefault: "Default",
		ioBestEffort: "Best effort",
		ioIdle: "Idle",
		level: "Level",
		levelPlaceholder: "4",
		levelHelper: "0 is the highest priority.",
		idleHelper:
			"Idle uses the disk only when nothing else does. Backups can take much longer.",
	},
	retention: {
		legend: "Retention",
		helper:
			"Applied per host to this policy's snapshots after each successful backup. Snapshots from lode runs and other policies are never touched.",
		none: "Keeps everything. Nothing is forgotten until you set a rule.",
		keepLast: "Keep last",
		keepWithin: "Keep within",
		keepWithinPlaceholder: "30d",
		keepDaily: "Keep daily",
		keepWeekly: "Keep weekly",
		keepMonthly: "Keep monthly",
		preview: {
			newPolicy: "The preview appears once the policy has snapshots.",
			working: "Calculating…",
			summary: (forget: string, total: string, hosts: string) =>
				`Would forget ${forget} of ${total} on ${hosts}.`,
			nothing: "Nothing would be forgotten right now.",
			suspicious: (n: number) =>
				n === 1
					? "1 suspicious snapshot is not counted and is never forgotten."
					: `${n} suspicious snapshots are not counted and are never forgotten.`,
			heavy: (pct: number) =>
				`This forgets ${pct}% of the snapshots. Check the rules before you save.`,
			savingNote:
				"Saving does not delete anything by itself. Snapshots are forgotten after the next successful backup of each host.",
			error:
				"Couldn't calculate the preview. Check that the server is reachable, then retry.",
			retry: "Retry loading",
			show: "Show snapshots to forget",
			hide: "Hide snapshots to forget",
			floor: (n: number) =>
				n === 1
					? "1 more snapshot is kept: the two newest complete snapshots of each host are never forgotten."
					: `${n} more snapshots are kept: the two newest complete snapshots of each host are never forgotten.`,
			forgetOf: (forget: number, total: number) =>
				`forget ${forget} of ${total}`,
			notKept: "Not kept by any rule",
			showAll: (n: number) => `Show all ${n} hosts`,
			more: (n: number) => `and ${n} more`,
		},
		confirm: {
			title: "Save retention rules?",
			body: (snapshots: string, hosts: string) =>
				`After the next successful backup of each host, ${snapshots} on ${hosts} will be forgotten. This cannot be undone. Their data stays in storage until garbage collection.`,
			confirm: "Save policy",
			keep: "Keep editing",
			more: (n: number) => `and ${n} more`,
		},
	},
	managed: {
		badge: "Managed",
		hint: (manager: string) =>
			`Managed by ${manager}. The next apply overwrites edits made here.`,
		alert: (manager: string) =>
			`Managed from Git. This policy is applied by ${manager}. The next apply overwrites changes made here.`,
	},
	validation: {
		path: 'Enter an absolute path that starts with "/".',
		script: 'Enter a file name from /etc/lodestore/hooks.d, without "/".',
		timeout: "Use a duration such as 90s, 30m or 2h.",
		nameTaken: (name: string) =>
			`A policy named "${name}" already exists. Choose another name.`,
	},
	matching: {
		title: "Matching hosts",
		summary: (n: number) =>
			`${n} ${n === 1 ? "host" : "hosts"} will be backed up`,
		skipped: (m: number) => `${m} use another repository and are skipped`,
		emptySelector: "No labels selected: every host in this repository matches.",
		zero: "No hosts match yet. Check the labels, or add a label to a host.",
		separator: " · ",
		show: "Show hosts",
		hide: "Hide hosts",
		hookWarnings: (n: number) =>
			n === 1 ? "1 hook warning" : `${n} hook warnings`,
		otherRepo: "Other repository",
		usesRepo: (repo: string) => `Uses ${repo}`,
		showAll: (n: number) => `Show all ${n} hosts`,
		invalid: "Fix the errors in the policy to see matching hosts.",
		loadFailed:
			"Couldn't load matching hosts. Check that the server is reachable, then retry.",
		hookMissing: (script: string) =>
			`Script ${script} is not installed on this host`,
		hookRejected: (script: string) =>
			`Script ${script} must be owned by root and not writable by group or others on this host`,
		inlineOff:
			"Inline commands are turned off on this host (hooks.allow_inline)",
		hookWarning: (text: string) => `Hook warning: ${text}`,
	},
	run: {
		failed: "run the policy",
		title: (policy: string) => `Run "${policy}" now?`,
		body: "Starts one job on each host below. Offline hosts start when they connect, within 24 hours.",
		confirm: (n: number) => (n === 1 ? "Run on 1 host" : `Run on ${n} hosts`),
		dismiss: "Don't run",
		nothing: "No hosts match this policy yet.",
		more: (n: number) => `and ${n} more`,
		skipped: (n: number) =>
			`${n} ${n === 1 ? "host uses" : "hosts use"} another repository and ${n === 1 ? "is" : "are"} skipped.`,
	},
	leave: {
		title: "Leave without saving?",
		body: "Your changes to this policy are not saved.",
		keep: "Keep editing",
		leave: "Leave without saving",
	},
	remove: {
		title: "Delete policy",
		heading: (name: string) => `Delete policy "${name}"?`,
		body: 'Hosts stop being backed up by it. Snapshots it already created stay in the repository, are marked "Policy deleted" and are never forgotten automatically.',
		managed:
			"It is managed from Git and comes back on the next apply unless you remove it from the files.",
		confirm: "Delete policy",
		keep: "Keep policy",
		blocked: (name: string, n: number) =>
			`Policy "${name}" has ${n} ${n === 1 ? "job" : "jobs"} in progress. Cancel them first, then delete the policy.`,
		toast: (name: string) => `Policy "${name}" deleted.`,
	},
} as const;
