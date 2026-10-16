import type { SnapshotItem } from "@/api/gen/model";
import { copy } from "@/copy";
import { displayName } from "@/lib/names";

const SHORT_ID = 8;

export const shortId = (id: string) => id.slice(0, SHORT_ID);

/** Mirrors the server's archive file name: `{host}-{id8}.{ext}`. */
export const archiveName = (hostname: string, id: string, ext: string) =>
	`${hostname.replace(/[^A-Za-z0-9.-]/gu, "_") || "snapshot"}-${shortId(id)}.${ext}`;

export const snapshotSource = (snapshot: SnapshotItem) =>
	snapshot.policy_name != null
		? displayName(snapshot.policy_name)
		: snapshot.stdin_name
			? copy.restore.cliSource(displayName(snapshot.stdin_name))
			: copy.restore.cliBadge;

export const snapshotFiles = ({ stats }: SnapshotItem) =>
	stats.files_new + stats.files_changed + stats.files_unchanged;
