export const presets = {
	legend: "Storage provider",
	local: "Local path",
	localMechanism: "A path the agents can reach",
	serverIssued: "Server issues a SAS",
	cannotLimit: "Cannot limit · use bucket lock",
	notVerified: " · not verified",
	mechanism: {
		"aws-s3": "Policy without delete",
		minio: "mc user without delete",
		wasabi: "Policy without delete",
		"backblaze-b2": "Key without deleteFiles",
		"hetzner-object-storage": "Bucket policy",
		"hetzner-storage-box": "Sub-account",
		"google-cloud-storage": "Role without delete",
		sftp: "Chrooted user, deny list",
	} as Record<string, string>,
	loadFailed: "Could not load providers",
	loadFailedBody: "Check that the server is reachable, then retry.",
	retry: "Retry",
};

export const connection = {
	heading: (label: string) => `Connect ${label}`,
	endpointHelper:
		"Leave empty for AWS; enter the URL of any other S3-compatible service.",
	endpointResult: "Endpoint",
	placeholder: {
		region: "Region",
		account_id: "Account ID",
		user: "Storage Box user",
	} as Record<string, string>,
	container: "Container",
	accountName: "Storage account name",
	azureEndpointHelper:
		"Leave empty for https://<account>.blob.core.windows.net",
	credentialKind: "Server credential",
	accountKey: "Account key",
	serverSas: "SAS token",
	folder: "Folder",
	gcsCredential: "Service account key",
	gcsCredentialHelper: "The key file in base64: base64 -w0 key.json",
	address: "Address",
	addressHelper: "host:port",
	directory: "Directory",
	directoryHelper: "An absolute path on the server.",
	user: "User",
	privateKey: "Private key",
	serverKeyTitle: "Server key",
	serverKeyBody:
		"Full access. Stays on the server; used for checks and cleanup.",
	verifyTitle: "Verify access",
	verifyBody:
		"Lodestore tries each permission once with a small test object, then removes it with the server key.",
};

export const rights = {
	title: "Give agents a key that cannot delete",
	lead: "A host that is broken into can then add backups but not erase them.",
	create: (label: string) => `Create the key in ${label}`,
	paste: "Paste the key it printed",
	check: "Check what the key can do",
	checkBody:
		"Lodestore writes a test object twice and tries to delete it with this key.",
	checkButton: "Check",
	checkAgain: "Check again",
	serverToo: "Server account needs this too",
	commands: "Commands for the new key",
	loadFailed: "Could not load the commands",
	retry: "Retry",
	agentCredential: "Service account key",
	agentUser: "User",
	agentPrivateKey: "Private key",
	agentSas: "SAS token",
	azure:
		"Lodestore signs a short SAS without delete for every agent session. Nothing to run.",
	skippedBodyShared:
		"Agents will use the server's credentials. Only a new repository with a limited key can change that.",
	bucketGroup: "Bucket",
	facts: {
		versioning: "Versioning",
		versioningOn: "Overwrites keep the earlier version",
		versioningOff: "Off: an overwrite replaces the object",
		lifecycle: "Lifecycle rules",
		lifecycleOk: "No rule deletes objects under this prefix",
	},
	skip: "Skip, use the server key",
	useLimited: "Use a limited key instead",
	skippedBody:
		"Agents will use the server key. You can limit it later from the repository page.",
	r2Title: "R2 tokens cannot be limited",
	result: "Result",
	resultIdle: "Run the check to see what this key can do.",
};

export const hostKey = {
	get: "Get host key",
	getting: "Getting the host key…",
	getFailed: "get the host key",
	title: "Is this your server?",
	compare: (command: string) =>
		`The server presented this host key. Run ${command} on it and compare.`,
	command: "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub",
	matches: "It matches",
	mismatch: "It does not match",
	mismatchBody:
		"Do not continue: this may not be your server. Check the address and the server's host key.",
	recheck: "Check host key again",
	recheckFailed: "check the host key",
	same: "The server presents the pinned key.",
	changedTitle: "The server's host key changed",
	pinned: "Pinned",
	presented: "Now",
	changedBody:
		"If nobody changed the server's key, someone may be between Lodestore and the server. Keep the pinned key and check the server.",
	keep: "Keep the pinned key",
	trust: "Trust the new key",
	trusted: "Agents now trust the new host key.",
};

export const limit = {
	title: "Limit agent rights",
	lead: {
		s3: "Create an access key that cannot delete, then paste it. Agents switch to it after the check passes.",
		gcs: "Create a service account that cannot delete objects, then paste its key. Agents switch to it after the check passes.",
		sftp: "Create an SFTP user that cannot remove or rename files, then paste its key. Agents switch to it after the check passes.",
		azblob:
			"Paste a SAS token that cannot delete. Agents switch to it after the check passes.",
		r2: "An R2 token cannot be limited. Turn on a bucket lock in Cloudflare so that objects cannot be deleted or replaced.",
	},
	cancel: "Cancel",
	submit: "Check and switch agents",
	checking: "Checking…",
	done: (provider: string) =>
		`Agents switched to the new key. Disable the old one at ${provider}.`,
	doneShared: "Agents switched to the new key. The server keeps its own key.",
	needsUpdate: (count: number) =>
		count === 1
			? "1 online agent is too old to take the new key; update it."
			: `${count} online agents are too old to take the new key; update them.`,
	failed: "switch the agents' key",
	close: "Close",
};

export const revoke = {
	stillWrites: (host: string, repo: string) =>
		`${host} can still write to ${repo} until the agent key is replaced.`,
	replaceKey: "Replace agent key",
	later: "Later",
	endsBy: (time: string) => `Its access ends by ${time}.`,
	sharedHolds: (host: string, repo: string) =>
		`${host} holds the server's own credentials for ${repo}. Only new credentials at the provider close that.`,
};
