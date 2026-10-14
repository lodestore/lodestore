export const copy = {
	nav: {
		overview: "Overview",
		hosts: "Hosts",
		policies: "Policies",
		jobs: "Jobs",
		restore: "Restore",
		settings: "Settings",
	},
	soon: "Soon",
	stubTitle: (section: string) => `${section} is not available yet`,
	stubBody:
		"This section arrives in a later release. Nothing else is needed from you now.",
	signOut: "Sign out",

	login: {
		title: "Sign in to Lodestore",
		button: "Sign in",
		error: "Incorrect username or password.",
		rateLimited: (n: number) => `Too many attempts. Try again in ${n} seconds.`,
		noAdmin:
			'No administrator exists yet. Open the setup link printed in the server log (look for "/setup?token=").',
		sessionExpired: "Your session expired. Sign in again to continue.",
	},
	setup: {
		title: "Create the administrator",
		button: "Create administrator",
		invalidLink:
			"This setup link is invalid or was already used. Restart the server to print a new link, or set LODESTORE_ADMIN_PASSWORD.",
		passwordRule: "At least 12 characters.",
		passwordMismatch: "Passwords do not match.",
		confirmPassword: "Confirm password",
	},
	unencryptedBanner: {
		title: "This connection is not encrypted",
		dismiss: "Dismiss unencrypted connection warning",
		body: "Passwords, keys and the recovery key you enter here travel over plain HTTP. Put the server behind a TLS proxy before you use it on a network.",
	},
	unlockKeyDismiss: "Dismiss unlock key warning",
	unlockKeyBanner:
		"The server's unlock key is stored next to the database. Anyone who can read the data directory can read your repositories. Move the key out with LODESTORE_UNLOCK_KEY_FILE or a secret store.",

	wizard: {
		title: "Set up your first repository",
		subtitle:
			"Four steps: choose storage, check access, save the recovery key, add a host. About 5 minutes.",
		steps: ["Storage", "Access", "Recovery key", "First host"],
		back: "Back",
		continueToAccess: "Continue to access",
		continueToKey: "Continue to recovery key",
		createRepository: "Create repository",
		openHosts: "Open Hosts",
		skipHost: "Skip adding a host",
		keepHost: "Keep host",
		discardChanges: "Discard changes",
		keepCurrentKey: "Keep current key",
	},
	backend: {
		s3: "S3-compatible storage. AWS S3, MinIO, Ceph, R2 and others.",
		local: "Local folder. A path both the server and the agent can see.",
		localNote:
			"Suitable for development or a single host. The server cannot guarantee that agents are unable to delete here.",
		experimental: "Experimental",
		moreBackends:
			"More backends (experimental): Azure Blob, Google Cloud Storage, SFTP, WebDAV. These are not covered by our storage tests.",
		twoKeys:
			"Two keys. The server key has full access and manages the repository. The agent key can only write, so a compromised host cannot delete backups.",
	},
	policy: {
		title: "Policy for the agent key",
		body: "Attach this to the write-only user, replacing the bucket name. Copy it as is.",
		tabIam: "AWS IAM",
		tabMinio: "MinIO",
	},
	check: {
		button: "Check storage",
		running: "Checking storage...",
		rowRunning: "Checking...",
		rows: {
			write: "Write objects",
			list: "List objects",
			conditional_write: "Conditional write (If-None-Match)",
			agent_delete: "Agent key permissions",
			object_lock: "Object Lock",
		},
		detail: {
			writePass: "The server key can write.",
			listPass: "The server key can list objects.",
			conditionalPass: "The backend rejects overwriting existing objects.",
			conditionalWarn:
				"Not supported. Lodestore still works but cannot use a conditional write as a second safety net.",
			agentKeyPass: "The agent key was refused when it tried to delete.",
			agentKeyBriefRow: "See the warning above.",
			agentKeyCanDeleteHeading: "The agent key can delete objects",
			agentKeyCanOverwriteHeading: "The agent key can overwrite objects",
			agentKeyCanDeleteBody: "A compromised host could delete backups.",
			agentKeyCanOverwriteBody:
				"It cannot delete, but a compromised host could replace backup data with garbage.",
			agentKeyLockOff:
				"Tighten the key's policy, or enable bucket versioning or Object Lock, and check again.",
			agentKeyLockOn:
				"Object Lock is enabled, but whether earlier versions are protected depends on its retention rules. Tightening the key's policy is still recommended.",
			agentKeyAccept:
				"You can also continue and accept the risk. This warning stays visible on the repository.",
			objectLockOn: "Object Lock is enabled on the bucket.",
			objectLockOff:
				"Not enabled. Optional, but recommended for ransomware protection.",
			objectLockNotApplicable: "Object Lock does not apply to this backend.",
			objectLockNotSupported: "This storage does not support Object Lock.",
			localDelete: "Not enforceable on a local folder.",
			fail: (backendError: string) =>
				`${backendError}. Check the storage settings and the key permissions, then check again.`,
		},
		status: {
			pass: "Passed",
			warn: "Warning",
			fail: "Failed",
			neutral: "Not applicable",
			stale: "Outdated",
		},
		blockedContinue: "Fix the failed checks to continue to the recovery key.",
	},
	existingRepo: {
		heading: "A Lodestore repository already exists here",
		body: "Connect it with its password or recovery key. Existing backups stay intact. The server adds its own key to the repository.",
		button: "Connect repository",
		error:
			"That password or recovery key does not open this repository. Check it and try again.",
		connected: "Repository connected.",
		tabPassword: "Password",
		tabRecoveryKey: "Recovery key",
	},
	recoveryKey: {
		heading: "Save your recovery key",
		body: "This is the only way to open the repository if you lose the password and the server. It is shown once and never stored by the server in readable form.",
		copy: "Copy recovery key",
		download: "Download .txt",
		confirmLabel: "Type the last group of the key",
		confirmHelper:
			"We check that you saved it. The repository is created only after this matches.",
		mismatch:
			"That does not match the last group of the key. Check the saved copy.",
		cta: "Create repository",
		goingBack: "Going back generates a new key.",
		passwordLabel: "Repository password",
		passwordHelper:
			"Opens the repository together with the recovery key. At least 12 characters.",
		created: "Repository created.",
		createdNamed: (name: string) => `Repository "${name}" created.`,
	},
	hosts: {
		title: "Hosts",
		addHost: "Add host",
		emptyHeading: "No hosts yet",
		emptyBody:
			"Add a host to start protecting it. Installing the agent takes about a minute.",
		revokedEmptyHeading: "No revoked hosts",
		revokedEmptyBody:
			"Hosts you revoke appear here. They stay listed so their history is not lost.",
		searchEmptyHeading: (query: string) => `No hosts match "${query}"`,
		searchEmptyBody:
			"Check the spelling, or search by label, for example env=prod.",
		searchPlaceholder: "Search by name or label",
		filterActive: "Active",
		filterRevoked: "Revoked",
		status: { online: "Online", offline: "Offline", revoked: "Revoked" },
		lastSeen: (relative: string) => `Last seen ${relative}`,
		showAllFilesystems: (n: number) => `Show all ${n} filesystems`,
		versionDiffers: (serverVersion: string) =>
			`Agent version differs from the server (v${serverVersion})`,
		expired: "Expired",
		editLabels: "Edit labels",
		saveLabels: "Save labels",
		labelsSaved: "Labels saved.",
		revokeHost: "Revoke host",
		hostRevoked: (name: string) => `Host "${name}" revoked.`,
		revokeTitle: (name: string) => `Revoke host "${name}"?`,
		revokeBody:
			"The agent is disconnected immediately and can't reconnect or receive jobs. To use this machine again, reinstall the agent with --force and a new token. The agent S3 key is shared by all hosts of this repository, so a revoked host can still write to the bucket until you replace the key. It cannot read or delete.",
		replaceKeyLink: "Replace the agent key",
		actionsFor: (name: string) => `Actions for ${name}`,
		columns: {
			host: "Host",
			status: "Status",
			lastSeenSort: "Last seen",
			version: "Version",
			platform: "Platform",
			labels: "Labels",
			certificate: "Certificate",
			actions: "Actions",
		},
		viewDetails: "View details",
		previous: "Previous",
		next: "Next",
		showing: (from: number, to: number, total: number) =>
			`Showing ${from}-${to} of ${total}`,
		details: {
			overview: "Overview",
			system: "System",
			storage: "Storage",
			certificate: "Certificate",
			status: "Status",
			lastContact: "Last contact",
			enrolled: "Enrolled",
			agentId: "Agent ID",
			copyAgentId: "Copy agent ID",
			repository: "Repository",
			os: "OS",
			arch: "Architecture",
			distro: "Distribution",
			kernel: "Kernel",
			uptime: "Uptime",
			path: "Path",
			type: "Type",
			free: "Free",
			used: "Used",
			notBefore: "Valid from",
			notAfter: "Valid until",
			nextRenewal: "Next renewal",
			description: "Host details",
		},
	},
	addHost: {
		title: "Add host",
		helper: (grpcPublicAddress: string) =>
			`Run the command as root on a Linux host (x86_64 or arm64) with systemd. The host needs outbound access to ${grpcPublicAddress}.`,
		labelsHelper:
			"Labels select which policies apply to this host. Example: env=prod. You can edit them later.",
		generate: "Generate token",
		generateNew: "Generate new token",
		shownOnce:
			"This command contains a one-time secret. It works once and expires in 1 hour. If you close this dialog you need a new token.",
		commandCopied: "Command copied",
		waiting: "Waiting for the host to connect...",
		connected: (name: string, version: string) =>
			`${name} is online. Version ${version}.`,
		tokenExpired: "The token expired. Generate a new one.",
		repository: "Repository",
		labelsTitle: "Labels",
		copyCommand: "Copy install command",
		commandObject: "install command",
		expiresIn: (remaining: string) => `Expires in ${remaining}`,
		installOptionsTitle: "Install options",
		installOptions:
			"--from-github downloads the agent from GitHub instead of this server. --force re-registers a host that is already registered. --uninstall removes the agent; add --purge to delete its config and state.",
		finish: "Finish host setup",
		noRepoHeading: "Create a repository first",
		noRepoBody:
			"Hosts back up into a repository. Add one to generate a join token.",
		noRepoButton: "Add repository",
		noAgentBinaries:
			"This server has no agent binaries. Add --from-github to the command, or put the binaries into the server's agent directory.",
	},
	labels: {
		validation:
			'Keys and values: up to 63 characters, letters, digits, "-", "_" and "."; key is required.',
		duplicateKey: "Keys must be unique.",
		tooMany: "At most 20 labels are allowed.",
		key: "Key",
		value: "Value",
		add: "Add label",
		remove: "Remove label",
	},
	repos: {
		title: "Repositories",
		add: "Add repository",
		emptyHeading: "No repositories yet",
		emptyBody: "Create a repository to give your hosts somewhere to back up.",
		agentKeyVerified: "Write-only verified",
		agentKeyCanDelete: "Can delete",
		agentKeyCanOverwrite: "Can overwrite",
		agentKeyNotChecked: "Not checked",
		replaceKeyTitle: (name: string) => `Replace the agent key for "${name}"?`,
		replaceKeyBody: (online: string) =>
			`Enter the new write-only key. The server verifies it, then sends it to ${online}. Offline agents receive it when they reconnect, and until then they cannot write.`,
		replaceKeyConfirm: "Verify and replace key",
		replaceKeyResult: (online: string, offline: string) =>
			`New key sent to ${online}. ${offline} will receive it when they reconnect.`,
		replaceKeyFailure: (reason: string) =>
			`The new key could not be verified: ${reason}. The current key is unchanged.`,
		replaceKey: "Replace agent key",
		accessKeyId: "Access key ID",
		secretAccessKey: "Secret access key",
		columns: {
			name: "Name",
			backend: "Backend",
			location: "Location",
			hosts: "Hosts",
			agentKey: "Agent key",
			lastChecked: "Last checked",
		},
		actionsColumn: "Actions",
		viewRepository: "View repository",
		checkAgain: "Check storage again",
		storageChecks: "Storage checks",
		lastCheckedAt: "Last checked",
		neverChecked: "These checks have not run yet.",
		agentKeyTitle: "Agent key",
		localAgentKey:
			"Agents write straight to the local folder; there is no separate agent key.",
		hostsTitle: "Hosts",
		breadcrumbSettings: "Settings",
		loadFailed:
			"Couldn't load this repository. Check that the server is reachable, then retry.",
		checkFailed: "storage check",
		keyChangeFailed: "replace the agent key",
		close: "Close",
	},
	errors: {
		list: (what: "hosts" | "repositories") =>
			`Couldn't load ${what}. Check that the server is reachable, then retry.`,
		hostDetails: "Couldn't load host details.",
		retry: "Retry loading",
		submit: (action: string, serverMessage?: string) =>
			`Couldn't ${action}. ${serverMessage ? `${serverMessage}. ` : ""}Try again.`,
	},
	common: {
		copy: (object: string) => `Copy ${object}`,
		copied: "Copied",
		copyButton: "Copy",
		copyFailed:
			"Could not copy automatically. The text is selected: press Ctrl+C (Cmd+C on a Mac) to copy it.",
		copyFailedUnselected:
			"Could not copy automatically. Select the text and press Ctrl+C (Cmd+C on a Mac) to copy it.",
		checking: "Checking...",
		dash: "—",
	},
} as const;
