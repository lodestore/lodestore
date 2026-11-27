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
		mode: { snapshot: "Snapshot", suspend: "Suspend", stop: "Stopped" },
		modeLabel: "Mode",
		volumes: "Volumes",
		userIds: {
			label: "User IDs",
			unprivileged: (base: number | null | undefined) =>
				base == null ? "Unprivileged" : `Unprivileged, 0 → ${base}`,
			privileged: "Privileged",
		},
		container: (vmid: number | null | undefined, name?: string | null) =>
			vmid == null ? "CT" : name ? `CT ${vmid} · ${name}` : `CT ${vmid}`,
		volume: (key: string, size: string) => `${key} ${size}`,
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
			`This folder is outside the folders this host restores into: ${roots}. Choose a folder under one of them.`,
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
		ownersShifted: (vmid: number | null | undefined, base: number) =>
			`${vmid == null ? "This container" : `CT ${vmid}`} is unprivileged, so owners are shifted on the node: uid 0 becomes ${base}. Copy the files into the container with cp -a and they belong to the right users.`,
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
	container: {
		open: "Restore container",
		title: (vmid: number | null | undefined, name?: string | null) =>
			vmid == null
				? "Restore container"
				: `Restore CT ${vmid}${name ? ` · ${name}` : ""}`,
		from: "From",
		fromValue: (node: string, id: string, when: string, size: string) =>
			`${node} · snapshot ${id} · ${when} · ${size}`,
		restoreAs: "Restore as",
		asNew: "New container",
		asReplace: "Replace an existing container",
		node: "Node",
		storage: "Storage",
		vmid: "VMID",
		replaceTarget: "Container",
		choose: "Choose",
		nextFree: (n: number, cluster: string, name?: string | null) =>
			`${n} is the next free ID in ${cluster}.${name ? ` Name: ${name}.` : ""}`,
		vmidTaken: (n: number) =>
			`VMID ${n} is taken. Pick another or leave the suggested one.`,
		vmidInvalid: "Enter a VMID from 100 to 999999999.",
		pluginMissing: "Plugin not installed",
		offline: "Offline",
		noStorage: "No storage for containers",
		effectsTitle: "The new container",
		effects: {
			stopped: "Stays stopped and won't start on boot",
			newMacs: "Gets new MAC addresses",
			unprivileged: "Runs unprivileged",
		},
		dropped: (n: number) =>
			`${n} config ${n === 1 ? "key" : "keys"} won't be restored`,
		droppedNone: "All config keys are restored.",
		droppedMore: (n: number) => `Show ${n} more`,
		droppedHide: "Show fewer",
		droppedShow: "Show",
		agentFilters: (node: string) =>
			`The agent on ${node} applies the same filter and lists what it dropped in the job.`,
		previewFailed:
			"Couldn't preview the config. The agent still filters it and lists what it dropped in the job.",
		reasons: {
			hookscript: "Runs a script as root on the node",
			lxc: "Raw LXC settings can give root on the node",
			bind_mount: "Bind mount of a node folder",
			device: "Passes a node device into the container",
			privileged: "Would change the container's privileges",
			features: "Container features that reach the node",
			args: "Extra arguments for the container runtime",
			network:
				"The interface uses a bridge that this node doesn't have, or a setting Lodestore doesn't restore",
			firewall: "Firewall rules aren't restored. Add them in Proxmox",
			unknown: "Lodestore doesn't know this setting",
			section: "Snapshot sections and pending changes aren't restored",
		},
		network: "Network interfaces kept",
		networkNotChecked: "Bridges are checked on the node when the restore runs",
		replaceOffHead: (node: string) =>
			`Replacing is turned off on ${node}. To allow it, run`,
		replaceOffCommand: "lodestore-agent pve install --allow-guest-in-place",
		replaceOffTail: "on the node.",
		replaceUnsigned:
			"An unsigned snapshot cannot replace an existing container.",
		unverifiedConfirm: "Restore this unsigned snapshot as a new container",
		replaceNone: "No container on this cluster can be replaced.",
		replaceWarningTitle: (vmid: number, name?: string | null) =>
			`All volumes of CT ${vmid}${name ? ` · ${name}` : ""} are replaced with the snapshot`,
		replaceWarningBody: (vmid: number, n: number) =>
			`VMID ${vmid} and its MAC addresses stay. Config comes from the snapshot${n > 0 ? ` without the ${n} ${n === 1 ? "key" : "keys"} listed below` : ""}.`,
		replaceDestroyed: "Deleted with no copy in this snapshot:",
		replaceDestroyedRow: (
			key: string,
			mp: string | undefined,
			size: string | undefined,
		) =>
			`${key}${mp ? ` ${mp}` : ""}${size ? ` · ${size}` : ""} — excluded from backups, not in this snapshot`,
		replaceConfirm: (
			vmid: number,
			name: string | null | undefined,
			node: string,
		) => `Replace CT ${vmid}${name ? ` · ${name}` : ""} on ${node}`,
		replaceButton: (vmid: number) => `Replace CT ${vmid}`,
		targetLabel: (
			vmid: number,
			name: string | null | undefined,
			node: string,
			state: string,
		) => `CT ${vmid}${name ? ` · ${name}` : ""} on ${node} · ${state}`,
		stopped: "stopped",
		running: "running",
		haManaged: "managed by HA",
		otherGuest: (vmid: number) =>
			`This snapshot is of another container than CT ${vmid}. Replacing it puts that container's data into CT ${vmid}.`,
		otherGuestUnknown:
			"This snapshot doesn't say which container it came from. Check that it is the right one before you replace.",
		factsStale:
			"The node hasn't reported its containers recently. Retry in a minute.",
		targetUnverifiable: (vmid: number) =>
			`CT ${vmid} has too many volumes to check. Replace it in Proxmox instead.`,
		retry: "Retry",
		confirmNew: (vmid: number | string) => `Restore as CT ${vmid}`,
		nodesFailed: "Couldn't load the nodes of this cluster.",
		failed: "start the restore",
		queued: (node: string) => `Restore queued for ${node}.`,
		dismiss: "Cancel",
	},
	cliSource: (name: string) => `CLI · ${name}`,
	cliBadge: "CLI",
} as const;
