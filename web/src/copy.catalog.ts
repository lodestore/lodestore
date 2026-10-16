import { snapshotCount } from "@/lib/plural";
import { sentence } from "@/lib/sentence";

export const catalog = {
	title: "Catalog",
	description:
		"The catalog is a cache of the snapshots in storage. It can be rebuilt from the bucket at any time.",
	lastSynced: "Last synced",
	neverSynced: "Not synced yet",
	snapshots: "Snapshots",
	rebuild: "Rebuild catalog",
	phase: {
		listing: "Listing snapshots in storage",
		reading: "Reading snapshots",
	},
	progress: (read: number, total: number) => `${read} of ${total} snapshots`,
	appearAsRead: "Snapshots appear in Restore as they are read.",
	openRestore: "Open Restore",
	skipped: (k: number) =>
		`${snapshotCount(k)} could not be read and ${k === 1 ? "was" : "were"} skipped. Their data is still in storage.`,
	skippedList: "Skipped snapshots",
	skippedMore: (n: number) => `and ${n} more`,
	failed: (message?: string | null) =>
		`Couldn't rebuild the catalog. ${sentence(message)}Check the storage access, then rebuild again.`,
	banner: (repo: string, read: number, total: number) =>
		`Rebuilding the catalog of ${repo}. ${read} of ${total} snapshots read.`,
	bannerListing: (repo: string) =>
		`Rebuilding the catalog of ${repo}. Listing snapshots in storage.`,
	viewProgress: "View progress",
	done: (n: number) => `Catalog rebuilt: ${snapshotCount(n)}.`,
	viewReport: "View report",
	connectedExisting:
		"Rebuilding the catalog from storage. Snapshots appear in Restore as they are read.",
};
