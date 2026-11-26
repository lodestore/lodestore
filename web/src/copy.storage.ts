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
	result: "Result",
	resultIdle: "Run the check to see what this key can do.",
};
