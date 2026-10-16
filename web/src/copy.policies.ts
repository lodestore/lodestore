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
		hosts: "Hosts",
		lastRun: "Last run",
		actions: "Actions",
	},
	allHosts: "All hosts",
	pickedHosts: "Selected hosts",
	neverRun: "Never run",
	otherRepos: (n: number) => `+${n} in other repositories`,
	moreLabels: (n: number) => `+${n}`,
	runNowDisabled: "No hosts match this policy yet.",
	subtitle: (repo: string, hosts: string) => `Repository ${repo} · ${hosts}`,
	moreActions: "More actions",
	editor: {
		tabForm: "Form",
		tabYaml: "YAML",
		yamlLabel: "Policy YAML",
		yamlNote: "Editing in the form rewrites the YAML and removes comments.",
		yamlBlocked: "Fix the YAML errors before switching to the form.",
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
		body: "Hosts stop being backed up by it. Snapshots it already created stay in the repository.",
		confirm: "Delete policy",
		keep: "Keep policy",
		blocked: (name: string, n: number) =>
			`Policy "${name}" has ${n} ${n === 1 ? "job" : "jobs"} in progress. Cancel them first, then delete the policy.`,
		toast: (name: string) => `Policy "${name}" deleted.`,
	},
} as const;
