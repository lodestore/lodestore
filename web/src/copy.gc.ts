import { hostCount, markerCount, packCount } from "@/lib/plural";
import { sentence } from "@/lib/sentence";

export const gc = {
	title: "Free up space",
	beta: "Beta",
	description:
		"Retention only forgets snapshots. Their data stays in storage until a cleanup cycle deletes what no snapshot uses.",
	run: "Run cleanup",
	dryRun: "Dry run",
	runOff:
		"Turn on scheduled cleanup to run it by hand. Dry run is always available.",
	switchLabel: "Scheduled cleanup",
	switchHint:
		"The server deletes data that no snapshot uses. Dry run never deletes anything.",
	abort: "Abort cycle",
	startFailed: "start cleanup",
	startFailedTitle: "Cleanup didn't start",
	loadFailedTitle: "Status didn't load",
	turnOn: "turn cleanup on",
	turnOff: "turn cleanup off",
	abortFailed: "abort the cycle",
	loadFailed:
		"Couldn't load cleanup status. Check that the server is reachable, then retry.",
	status: {
		progressLabel: "Cleanup progress",
		off: "Cleanup is off",
		offLine: "Nothing is deleted from this repository.",
		idle: "Idle",
		nextRun: ["Next run ", "."],
		noSchedule: "No schedule. Run it by hand.",
		aborted: ["The last cycle was aborted ", ". Nothing was deleted."],
		dryRunTitle: "Dry run in progress",
		alsoDryRun: "A dry run is also in progress.",
		phase: {
			marking: "Checking snapshots",
			repacking: "Rewriting packs",
			waiting: "Marked for deletion",
			postponed: "Deletion is postponed",
			verifying: "Checking again before deleting",
			sweeping: "Deleting unused data",
		},
		snapshots: (done: number, total: number) => `${done} of ${total} snapshots`,
		repacked: (done: number, total: number, size: string) =>
			`${done} of ${total} packs · ${size}`,
		swept: (done: number, total: number) => `${done} of ${total} packs`,
		willDelete: (n: number, size: string, when: string) =>
			`${packCount(n)} (${size}) will be deleted after ${when}`,
		parens: ["(", ")."],
		pauseNote: "The pause lets running backups finish first.",
		pauseLabel: "Pause before deletion",
		postponedLine: (n: number) =>
			`${packCount(n)} ${n === 1 ? "is" : "are"} marked. Deletion waits for the backups listed above.`,
		verifying: "Comparing the marked packs with the current snapshots.",
		restoresLine: "Waiting for running restores to finish before deleting.",
		sweepNote: "This step can't be aborted.",
	},
	failure: {
		title: "Cleanup stopped",
		body: (reason: string) =>
			`Nothing was deleted. ${sentence(reason)}`.trimEnd(),
		hint: "Fix the cause, then run a dry run to check.",
		titleAfterDelete: "Cleanup stopped while deleting",
		bodyAfterDelete: (reason: string) =>
			`${sentence(reason)}It continues where it stopped, on the next run or when the server retries it. Nothing a snapshot needs was removed.`,
	},
	markers: {
		title: "Deletion is waiting for a backup that did not finish",
		body: "Cleanup does not delete anything until these backups finish or you unlock them.",
		source: " · lode backup",
		started: " · started ",
		unknownHost: "Unknown host",
		more: (n: number) => `and ${n} more`,
		unlock: (n: number) =>
			n === 1 ? "Unlock marker…" : `Unlock ${n} markers…`,
		dialogTitle: (n: number) => `Unlock ${markerCount(n)}?`,
		dialogBody: (hosts: string) =>
			`Cleanup stops waiting for: ${hosts}. If one of these backups is still running, its snapshot can lose data and fail to restore. Unlock only backups you know were interrupted.`,
		confirm: "Unlock markers",
		keep: "Keep waiting",
		done: (n: number) => `Unlocked ${markerCount(n)}. Cleanup continues.`,
		failed: "unlock the markers",
		failedTitle: "Markers weren't unlocked",
	},
	quarantine: {
		title: (n: number) =>
			n === 1
				? "Cleanup is stopped: 1 snapshot cannot be read"
				: `Cleanup is stopped: ${n} snapshots cannot be read`,
		description:
			"Each one was found in storage but does not open or points to data that is missing. Nothing is deleted until you review them.",
		more: (n: number) => `and ${n} more`,
		list: "Snapshots",
		bound:
			"This snapshot belongs to a backup job. Run a full check, then repair the repository or delete the snapshot.",
		exclude: "Exclude from cleanup…",
		dialogTitle: "Exclude this snapshot from cleanup?",
		dialogBody:
			"The next cleanup will no longer protect the data this snapshot points to, and may delete what only this snapshot uses. The snapshot object stays in storage; delete it separately if you no longer need it.",
		confirm: "Exclude snapshot",
		cancel: "Cancel",
		done: "The snapshot is excluded. Run cleanup again to continue.",
		failed: "exclude the snapshot",
	},
	log: {
		title: "Cycle log",
		more: (n: number) => `and ${n} more`,
		level: { info: "Info", warning: "Warning", error: "Error" },
	},
	preflight: {
		title: "Cleanup can't run yet",
		oldAgents: (n: number) =>
			`${hostCount(n)} ${n === 1 ? "runs" : "run"} an agent that does not support cleanup: `,
		oldAgentsEnd: ". Update them or revoke them.",
		more: (n: number) => `and ${n} more`,
		serverKey:
			"The server's storage key cannot delete objects. Cleanup needs delete permission on the bucket.",
		agentKey:
			"The agent key cannot read the locks/ folder. Replace the agent key policy so backups can see data marked for deletion.",
		openAgentKey: "Open agent key",
	},
	settings: {
		manualOnly: "manual only",
		cronHelper:
			"Five fields or @hourly, @daily, @weekly, @monthly. Leave empty to run cleanup only by hand.",
		header: "Settings",
		headerValues: (schedule: string, pause: string, pct: number) =>
			` · ${schedule} · pause ${pause} · repack below ${pct}%`,
		pause: "Pause before deletion",
		pausePlaceholder: "24h",
		pauseHelper: (floor: string) =>
			`Backups that started before the cycle must finish within this time. At least ${floor}.`,
		repack: "Repack below",
		percent: "%",
		repackHelper:
			"A pack with less live data than this is rewritten to free its space.",
		repackInvalid: "Enter a whole number from 1 to 99.",
		maxRepack: "Max repack per cycle",
		maxRepackPlaceholder: "10GiB",
		maxRepackHelper:
			"Limits how much data the server moves to repack in one cycle.",
		save: "Save settings",
		discard: "Discard changes",
		saved: "Cleanup settings saved. They apply from the next cycle.",
		saveFailed: "save the cleanup settings",
	},
	report: {
		lastCleanup: "Last cleanup",
		lastDryRun: "Last dry run",
		empty:
			"No report yet. Run a dry run to see how much space cleanup would free.",
		freed: (size: string) => `Freed ${size}.`,
		nothingFreed: "Nothing was freed.",
		wouldFree: (size: string) => `Would free ${size}.`,
		nothingToFree: "Nothing to free right now.",
		locked: (size: string) => `${size} more is held by Object Lock.`,
		finished: "Finished ",
		took: (duration: string) => ` · took ${duration}`,
		factFreed: "Freed",
		factWouldFree: "Would free",
		factDeleted: "Packs deleted",
		factToDelete: "To delete",
		factRepacked: "Packs repacked",
		factToRepack: "To repack",
		factTransfer: "Repack transfer",
		factLocked: "Held by Object Lock",
		freesAfter: (date: string) => `Frees after ${date}`,
		dateUnknown: "Release date unknown",
		download: "Download full report",
		downloadFailed: "Couldn't download the report. Try again.",
	},
};
