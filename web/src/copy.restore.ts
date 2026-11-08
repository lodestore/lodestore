import { isolate } from "@/lib/names";
import { fileCount, itemCount } from "@/lib/plural";
import { sentence } from "@/lib/sentence";

export const restore = {
	title: "Restore",
	lead: "Find a file across every host, or pick a host and browse its snapshots.",
	tabs: { byHost: "By host", find: "Find file" },
	hostPicker: {
		label: "Host",
		current: (host: string) => `Host: ${host}`,
		choose: "Choose a host",
		search: "Search hosts",
		hosts: "Hosts",
		noAgentGroup: "Hosts without an agent",
		noAgentGroupHint:
			"Snapshots from hosts that have not enrolled on this server.",
		noAgentBadge: "No agent",
		noSigned: "No signed snapshots",
		sourceHost: "Source host",
		noMatch: (query: string) => `No host matches "${query}".`,
		keepTyping: "Keep typing to narrow the list.",
		loadFailed:
			"Couldn't load hosts. Check that the server is reachable, then retry.",
	},
	noAgent: (host: string) =>
		`No agent is connected for ${isolate(host)}. You can browse its snapshots, download files, or restore them to another host.`,
	empty: {
		heading: "Nothing to restore yet",
		body: "Snapshots appear here once a backup finishes. Create a policy and run it on a host.",
		createPolicy: "Create policy",
		addRepository: "Add repository",
	},
	timeline: {
		title: "Snapshots",
		today: "Today",
		yesterday: "Yesterday",
		showOlder: "Show older snapshots",
		showing: (n: number, total: number) => `Showing ${n} of ${total} snapshots`,
		collapsed: (time: string, id: string) => `Snapshot ${time} · ${id}`,
		change: "Change snapshot",
		rebuilding: "More snapshots may still appear.",
		stopLabel: (id: string, when: string) => `Snapshot ${id} from ${when}`,
		selected: "Selected",
		emptyHeading: (host: string) => `No snapshots for ${isolate(host)} yet`,
		emptyBody: "Once a backup finishes, its snapshot is listed here.",
		noSignedBody:
			"Nothing signed any of this host's snapshots. Pick one from the list to restore it into another folder.",
		loadFailed:
			"Couldn't load snapshots. Check that the server is reachable, then retry.",
	},
	header: {
		title: "Snapshot",
		source: "Source",
		size: "Size",
		files: "Files",
		copyId: "Copy snapshot ID",
		incomplete: "Incomplete",
		incompleteBody: "Some files could not be read when this snapshot was made.",
		suspiciousTitle: "This snapshot looks unusual.",
		suspiciousBody:
			"Retention does not count it and does not forget it. Review it, then clear the mark or delete the snapshot.",
		unverifiedTitle: "Nothing signed this snapshot",
		unverifiedBody:
			"It may not come from this host. You can restore it into another folder or download it after confirming.",
		clearMark: "Clear mark…",
		deleteSnapshot: "Delete snapshot…",
	},
	snapshot: {
		openFailed: (message?: string) =>
			`Couldn't open this snapshot. ${sentence(message)}Try again, or choose another snapshot.`,
		goneHeading: "This snapshot no longer exists.",
		goneBody: "It was removed from the repository. Choose another snapshot.",
		back: "Back to snapshots",
	},
	browser: {
		pathLabel: "Path in snapshot",
		root: "Snapshot root",
		more: "Show hidden folders in the path",
		contents: (path: string) => `Contents of ${path}`,
		columns: {
			name: "Name",
			modified: "Modified",
			size: "Size",
			permissions: "Permissions",
			owner: "Owner",
		},
		openFolder: (name: string) => `Open folder ${name}`,
		linkTarget: (target: string) => `→ ${target}`,
		empty: "This folder is empty.",
		next: "Show next 200",
		previous: "Show previous 200",
		showing: (shown: string, total: string) =>
			`Showing ${shown} of ${total} items`,
		listFailed: (message?: string) =>
			`Couldn't list this folder. ${sentence(message)}Retry, or open a different folder.`,
		filter: {
			placeholder: "Filter by name",
			label: "Filter files by name",
			help: "Case-insensitive. Searches this folder and everything below it.",
		},
		search: {
			results: (query: string, path: string) =>
				`Results for "${isolate(query)}" in ${isolate(path)}`,
			clear: "Clear filter",
			searching: (n: number) => `Searching… ${n} found`,
			searchingStatus: "Searching…",
			limit:
				"Showing the first 500 matches. Narrow the filter or open a deeper folder.",
			noMatch: (query: string) => `No match for "${isolate(query)}"`,
			noMatchBody: (path: string) =>
				`Nothing in ${isolate(path)} or below has that name. Check the spelling, or filter from a higher folder.`,
			noMatchRoot:
				"Nothing in this snapshot has that name. Check the spelling.",
			failed: (message?: string) =>
				`Couldn't search this snapshot. ${sentence(message)}Try again, or filter from another folder.`,
			openParent: (path: string) => `Open folder ${path}`,
		},
	},
	selection: {
		selectAll: "Select all items in this folder",
		select: (name: string) => `Select ${name}`,
		selected: (n: number) => `${itemCount(n)} selected`,
		folders: "Folders include everything inside.",
		includedWith: (folder: string) => `Included with ${folder}`,
		clear: "Clear selection",
		hint: "Select files or folders to restore them or download an archive.",
		cleared: "Selection cleared",
		clearedSnapshot: "Selection cleared because the snapshot changed.",
	},
	actions: {
		download: "Download…",
		restore: "Restore…",
		noTarget: "Add a host to restore to.",
	},
	viewJob: "View job",
	restoreDialog: {
		title: (n: number) => `Restore ${itemCount(n)}`,
		suspiciousWarning:
			"This snapshot is marked suspicious. Files in it may be encrypted or damaged.",
		unverifiedConfirm: "Restore this unsigned snapshot into another folder",
		unverifiedInPlaceDisabled:
			"An unsigned snapshot cannot be restored over the original files.",
		inPlaceOff:
			"Restoring over the original files is turned off on this host. To allow it, set restore.allow_in_place: true in /etc/lodestore/agent.yaml, or run install.sh with --allow-in-place-restore.",
		outsideRoots: (roots: string) =>
			`This folder is outside the folders this host restores into: ${roots}. The restore will be refused; choose a folder under one of them.`,
		newFolderOff:
			"Restoring into a new folder is turned off on this host. To allow it, add a folder to restore.allowed_restore_roots in /etc/lodestore/agent.yaml.",
		from: "From",
		fromValue: (host: string, id: string, when: string) =>
			`${isolate(host)} · snapshot ${id} · ${when}`,
		target: "Restore to host",
		destination: "Destination",
		newFolder: "New folder",
		newFolderHelp: "Recommended. Nothing existing is touched.",
		folderOn: (host: string) => `Folder on ${host}`,
		original: "Original location",
		originalWarning: "Files are written to the paths they were backed up from.",
		originalConfirm: (host: string) =>
			`I understand this writes into the original paths on ${isolate(host)}.`,
		otherHost: (target: string, source: string) =>
			`${isolate(target)} will receive files from ${isolate(source)}.`,
		otherRepository: "It uses another repository.",
		offline: (target: string) =>
			`${isolate(target)} is offline. The restore starts when it connects, within 24 hours.`,
		conflict: "If a file already exists",
		conflictOptions: {
			overwrite_changed: "Overwrite changed files",
			skip_existing: "Skip existing files",
			stop_if_not_empty: "Stop if the folder is not empty",
		},
		owners: "Restore original owners",
		ownersShort: { on: "Original owners", off: "Agent user (root)" },
		options: "Options",
		optionsValue: (conflict: string, owners: string) =>
			`${conflict} · ${owners}`,
		counted: (files: number, size: string) => `${fileCount(files)} · ${size}`,
		counting: "Counting files…",
		countFailed: "Couldn't count the files. You can still start the restore.",
		pathInvalid: 'Enter an absolute path that starts with "/".',
		pathDotDot: 'Enter the path without "..". Name each folder on the way.',
		pathRoot:
			'Choose a folder other than "/". To write to the original paths, use Original location.',
		hostMissing: "Choose a host to restore to.",
		confirm: "Start restore",
		confirmInPlace: "Restore in place",
		dismiss: "Don't restore",
		failed: "start the restore",
		queued: (host: string) => `Restore queued for ${isolate(host)}.`,
	},
	downloadDialog: {
		title: (n: number) => `Download ${itemCount(n)}`,
		body: "The server decrypts the data as it streams the archive to your browser. Nothing is written to disk on the server.",
		unverifiedConfirm: "Download this unsigned snapshot",
		format: "Format",
		formats: {
			tar_gz: {
				ext: "tar.gz",
				help: "Compressed tar archive. Keeps permissions, owners, links and extended attributes.",
			},
			tar_zst: {
				ext: "tar.zst",
				help: "Zstandard-compressed tar. Smaller and faster than gzip, same metadata. Needs zstd to extract.",
			},
			zip: {
				ext: "zip",
				help: "Opens anywhere. Keeps files and modification times only.",
			},
		},
		fileName: "File name:",
		limits:
			"There is no size limit and no resume. If the download stops, start it again.",
		confirm: "Download",
		dismiss: "Don't download",
		failed: "start the download",
		started: "Download started. The archive streams to your browser.",
	},
	cliSource: (name: string) => `CLI · ${name}`,
	cliBadge: "CLI",
} as const;
