import { agentUpdate } from "@/copy.agentUpdate";
import { catalog } from "@/copy.catalog";
import { checks } from "@/copy.checks";
import { find } from "@/copy.find";
import { gc } from "@/copy.gc";
import { hostBackup, snapshots } from "@/copy.host";
import { jobs } from "@/copy.jobs";
import { notifications } from "@/copy.notifications";
import { overview } from "@/copy.overview";
import { policies } from "@/copy.policies";
import { restore } from "@/copy.restore";
import { tokens } from "@/copy.tokens";
import { user } from "@/copy.user";
import { sentence } from "@/lib/sentence";

export const copy = {
	agentUpdate,
	catalog,
	checks,
	find,
	gc,
	jobs,
	notifications,
	overview,
	policies,
	restore,
	tokens,
	user,
	hostBackup,
	snapshots,
	demo: {
		notAvailable: "Not available in the demo.",
		banner:
			"This is a public demo with synthetic data. Saving changes is turned off. You can still download an archive.",
	},
	nav: {
		overview: "Overview",
		hosts: "Hosts",
		policies: "Policies",
		jobs: "Jobs",
		restore: "Restore",
		settings: "Settings",
	},
	settingsNav: {
		label: "Settings sections",
		repositories: "Repositories",
		notifications: "Notifications",
		tokens: "API tokens",
		user: "User",
	},
	soon: "Soon",
	brand: "Lodestore",
	signOut: "Sign out",
	apiReference: "API reference",
	theme: { label: "Theme", light: "Light", dark: "Dark", system: "System" },

	shell: {
		navLabel: "Main navigation",
		groups: { workspace: "Workspace", backup: "Backup", system: "System" },
		skipLink: "Skip to content",
		toggleSidebar: "Toggle sidebar",
		searchTrigger: "Search…",
		commandPalette: "Command palette",
	},
	palette: {
		label: "Command palette",
		placeholder: "Search hosts, repositories and pages",
		groups: {
			pages: "Pages",
			hosts: "Hosts",
			repositories: "Repositories",
			actions: "Actions",
		},
		addHost: "Add host",
		addRepository: "Add repository",
		createPolicy: "Create policy",
		createApiToken: "Create API token",
		findFile: "Find file…",
		freeUpSpace: "Free up space",
		freeUpSpaceIn: (name: string) => `Free up space in ${name}`,
		cleanupKeywords: "cleanup clean gc garbage reclaim",
		lightTheme: "Switch to light theme",
		darkTheme: "Switch to dark theme",
		systemTheme: "Use system theme",
		empty: (query: string) =>
			`No results for "${query}". Try a host name or a label such as env=prod.`,
		hints: {
			move: "↑↓ to move",
			open: "↵ to open",
			close: "Esc to close",
		},
		hostsFailed: "Couldn't load hosts",
		repositoriesFailed: "Couldn't load repositories",
	},
	hostPage: {
		agent: { online: "Agent online", offline: "Agent offline" },
		tabs: { overview: "Overview", snapshots: "Snapshots", jobs: "Jobs" },
		editLabels: "Edit labels",
		revoke: "Revoke host",
		notFound: {
			heading: "Host not found",
			body: "It may have been removed, or the link is wrong.",
			back: "Back to Hosts",
		},
		revokedBanner:
			"This host is revoked. It cannot connect or receive jobs. Reinstall the agent with --force and a new token to use this machine again.",
		loadFailed:
			"Couldn't load this host. Check that the server is reachable, then retry.",
		cards: {
			status: "Status",
			system: "System",
			labels: "Labels",
			certificate: "Certificate",
			storage: "Storage",
		},
	},
	auth: {
		tagline: "Backups outlive the server.",
		loginLead: "Enter your administrator credentials.",
		setupLead: "Choose the credentials for the first administrator.",
		showPassword: "Show password",
		hidePassword: "Hide password",
		username: "Username",
		password: "Password",
	},
	access: {
		serverKey: {
			title: "Server key",
			body: "Full access. Stays on the server to manage and clean up the repository.",
		},
		agentKey: {
			title: "Agent key",
			body: "Write-only. Sent to every host so it can upload backups.",
		},
		writeOnlyBadge: "Write-only",
		whyWriteOnly: {
			title: "Why write-only?",
			body: "If a host is ever compromised, the attacker can add data but cannot delete or read your backups. Deleting data is done by the server only.",
		},
		verify: {
			title: "Verify access",
			body: "Lodestore tries each permission once with a small test object, then removes it with the server key.",
		},
	},
	checklist: {
		summary: (counts: { pass: number; warn: number; fail: number }) =>
			[
				counts.pass > 0 && `${counts.pass} passed`,
				counts.warn > 0 && `${counts.warn} with warnings`,
				counts.fail > 0 && `${counts.fail} failed`,
			]
				.filter(Boolean)
				.join(", "),
		why: {
			write:
				"Lodestore stores every backup through this write access. Check the server key's policy and the bucket name, then check again.",
			list: "Lodestore lists objects to find existing backups and to clean up. Allow listing for the server key, then check again.",
			conditionalWrite:
				"A conditional write stops two writers from replacing the same object. Without it, a race could overwrite backup data.",
			objectLock:
				"With Object Lock, stored backups cannot be changed or deleted until their retention ends, even with the server key.",
		},
		whyItMatters: "Why this matters",
		recommended: "Recommended",
		outdated: "Outdated",
	},
	recoverySteps: {
		save: "1. Save the key",
		password: "2. Choose a password",
		confirm: "3. Confirm you saved it",
		hint: "Store it in a password manager or print it. Do not keep it only on this server.",
		copied: "Copied",
		downloaded: "Downloaded",
		lastGroup: "Last group",
		completed: "completed",
	},
	firstHost: {
		whatNext: "What happens next",
		timeline: ["Run the command", "The agent enrolls", "The host appears here"],
		successHeading: (name: string) => `${name} is connected`,
		successDetail: (version: string) => `Online, version ${version}.`,
		nextStep: "Next: create a backup policy",
	},

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
		stepDescriptions: [
			"Choose where backups live",
			"Give Lodestore two keys",
			"Save the way back in",
			"Connect a machine",
		],
		progress: (n: number, total: number) => `Step ${n} of ${total}`,
		progressLabel: "Progress",
		railFootnote: "Your progress is kept until you reload the page.",
		exit: "Exit setup",
		discard: {
			title: "Leave without creating a repository?",
			body: "Nothing has been written to the bucket yet.",
			keep: "Keep editing",
			leave: "Leave without creating",
		},
		previousStep: "Previous step",
		newTitle: "Add a repository",
		completed: "completed",
	},
	backend: {
		s3: "S3-compatible storage such as AWS S3, MinIO or Ceph.",
		local: "Local folder. A path both the server and the agent can see.",
		experimental: "Experimental",
		twoKeys:
			"Two keys. The server key has full access and manages the repository. The agent key can only write, so a compromised host cannot delete backups.",
	},
	policy: {
		title: "Policy for the agent key",
		body: "Attach this to the agent key's user. The bucket name follows what you typed. Copy it as is.",
		tabIam: "AWS IAM",
		tabMinio: "MinIO",
	},
	check: {
		button: "Check storage",
		running: "Checking storage…",
		rowRunning: "Checking…",
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
			lastSeen: "Last seen",
			version: "Version",
			platform: "Platform",
			labels: "Labels",
			certificate: "Certificate",
			actions: "Actions",
		},
		tiles: {
			total: "Total hosts",
			online: "Agents online",
			offline: "Agents offline",
		},
		openRow: (name: string) => `Open ${name}`,
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
		labelsHelper:
			"Labels select which policies apply to this host. Example: env=prod. You can edit them later.",
		generate: "Generate token",
		generateNew: "Generate new token",
		shownOnce:
			"This command contains a one-time secret. It works once and expires in 1 hour. If you lose it, generate a new token.",
		commandCopied: "Command copied",
		waiting: "Waiting for the host to connect…",
		connected: (name: string, version: string) =>
			`${name} is online. Version ${version}.`,
		tokenExpired: "The token expired. Generate a new one.",
		repository: "Repository",
		labelsTitle: "Labels",
		copyCommand: "Copy install command",
		commandObject: "install command",
		expiresIn: (remaining: string) => `Expires in ${remaining}`,
		finish: "Finish host setup",
		noRepoHeading: "Create a repository first",
		noRepoBody:
			"Hosts back up into a repository. Add one to generate a join token.",
		noRepoButton: "Add repository",
		insecure: {
			warning:
				"Over plain HTTP anyone on the network can get root on this host.",
			verifyTitle: "Check the agent signature by hand",
			verifyIntro:
				"On the host, download the agent and its signature, and verify them with the Lodestore project key (key id 8CA38864EEA52F78, also in the documentation).",
			verifyAfter:
				"The last line of the output must read Trusted comment: lodestore-agent, then the version and the architecture of the host. Then run the install command above on the same host. With minisign installed, the installer checks the agent with the same key. Over plain HTTP the install script itself can be replaced on the way, so read it before you run it.",
			verifyObject: "verification commands",
			copyVerify: "Copy verification commands",
		},
		noAgentBinaries: {
			before: "This server has no agent binaries. Add ",
			after:
				" to the install command, or put the binaries into the server's agent directory.",
		},
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
			cleanup: "Cleanup",
		},
		cleanup: {
			off: "Off",
			manualOnly: "Manual only",
			next: "Next ",
			running: "Running",
			waiting: "Waiting",
			deletes: "deletes ",
			postponed: "Postponed",
			stopped: "Stopped",
			open: (name: string) => `Cleanup of ${name}:`,
		},
		actionsColumn: "Actions",
		actionsFor: (name: string) => `Actions for ${name}`,
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
		notFound: {
			heading: "Repository not found",
			body: "It may have been removed, or the link is wrong.",
			back: "Back to Repositories",
		},
		loadFailed:
			"Couldn't load this repository. Check that the server is reachable, then retry.",
		checkFailed: "storage check",
		keyChangeFailed: "replace the agent key",
		close: "Close",
	},
	errors: {
		list: (what: "hosts" | "repositories") =>
			`Couldn't load ${what}. Check that the server is reachable, then retry.`,
		page: "Couldn't open Lodestore. Check that the server is reachable, then retry.",
		retry: "Retry loading",
		submit: (action: string, serverMessage?: string) =>
			`Couldn't ${action}. ${sentence(serverMessage)}Try again.`,
		refused: (action: string, serverMessage?: string) =>
			`Couldn't ${action}. ${sentence(serverMessage)}`.trim(),
	},
	common: {
		justNow: "just now",
		copy: (object: string) => `Copy ${object}`,
		copied: "Copied",
		copyButton: "Copy",
		copyFailed:
			"Could not copy automatically. The text is selected: press Ctrl+C (Cmd+C on a Mac) to copy it.",
		showSecret: (what: string) => `Show ${what}`,
		hideSecret: (what: string) => `Hide ${what}`,
		copyFailedUnselected:
			"Could not copy automatically. Select the text and press Ctrl+C (Cmd+C on a Mac) to copy it.",
		dash: "—",
	},
} as const;
