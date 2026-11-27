import { HttpResponse, http } from "msw";
import type {
	ContainerFacts,
	DroppedKey,
	GuestRestoreBody,
	GuestTargets,
	ReplaceTarget,
	RestoreHost,
	RestoreSnapshot,
	TargetNode,
} from "@/api/gen/model";
import { apiError } from "@/test/app";
import { server } from "@/test/server";

export const CT_HOST = "agent-pve-01";
export const CT_SNAP = "c".repeat(64);
export const NODE_1 = "h-pve-01";
export const NODE_2 = "h-pve-02";

export const ctSnapshot: RestoreSnapshot = {
	id: CT_SNAP,
	hostname: "pve-01",
	host_key: CT_HOST,
	repository_id: "repo-1",
	time: Date.UTC(2026, 10, 26, 2, 10),
	incomplete: false,
	paths: [],
	policy_name: "proxmox-nightly",
	policy_deleted: false,
	received_at: Date.UTC(2026, 10, 26, 2, 11),
	verified_by: "server",
	source_type: "proxmox-ct",
	source_label: "CT 200 · dns",
	tags: [],
	warnings_total: 0,
	errors_total: 0,
	stats: {
		bytes_total: 22_960_000_000,
		bytes_added: 1024,
		bytes_read: 22_960_000_000,
		dirs: 3,
		files_new: 10,
		files_changed: 0,
		files_unchanged: 0,
	},
};

export const restoreHost: RestoreHost = {
	key: CT_HOST,
	host_id: NODE_1,
	hostname: "pve-01",
	agent: true,
	status: "online",
	labels: [],
	repository_id: "repo-1",
	repository_name: "main",
	snapshot_count: 1,
	latest_snapshot_ms: ctSnapshot.time,
	unverified_count: 0,
	restore_roots: ["/var/lib/lodestore/restore"],
};

export const dropped = (n: number): DroppedKey[] =>
	Array.from({ length: n }, (_, i) => ({
		key: i === 0 ? "lxc.apparmor.profile" : `lxc.key${i}`,
		value: i === 0 ? "unconfined" : `value-${i}`,
		reason: "lxc",
	}));

const NET0 = { key: "net0", value: "bridge=vmbr0,name=eth0,tag=40" };

export const containerFacts = (
	over: Partial<ContainerFacts> = {},
): ContainerFacts => ({
	bridges_checked: true,
	dropped: dropped(1),
	idmap: "u:0:100000:65536",
	kept_count: 8,
	mode: "snapshot",
	name: "dns",
	network: [NET0],
	root_host_uid: 100000,
	unprivileged: true,
	vmid: 200,
	volumes: [
		{ key: "rootfs", mp: "/", size_bytes: 8 * 1024 ** 3 },
		{ key: "mp0", mp: "/mnt/data", size_bytes: 32 * 1024 ** 3 },
	],
	...over,
});

const node = (name: string, over: Partial<TargetNode> = {}): TargetNode => ({
	agent: true,
	host_id: `h-${name}`,
	in_place_allowed: false,
	name,
	next_vmid: 105,
	online: true,
	plugin_installed: true,
	...over,
});

const target = (
	vmid: number,
	name: string,
	over: Partial<ReplaceTarget> = {},
): ReplaceTarget => ({
	ha_managed: false,
	host_id: NODE_1,
	name,
	node: "pve-01",
	other_guest: false,
	pve_uid: "uid-200",
	running: false,
	vmid,
	volumes: [
		{
			key: "rootfs",
			value: "local-lvm:vm-200-disk-0,size=8G",
			size_bytes: 8 * 1024 ** 3,
			destroyed_without_copy: false,
		},
	],
	volumes_truncated: false,
	...over,
});

export const withDestroyed = (vmid: number, name: string) =>
	target(vmid, name, {
		volumes: [
			{
				key: "rootfs",
				value: "local-lvm:vm-200-disk-0,size=8G",
				size_bytes: 8 * 1024 ** 3,
				destroyed_without_copy: false,
			},
			{
				key: "mp1",
				value: "local-lvm:vm-200-disk-1,mp=/cache,size=2T,backup=0",
				size_bytes: 2 * 1024 ** 4,
				destroyed_without_copy: true,
			},
			{
				key: "unused0",
				value: "local-lvm:vm-200-disk-2",
				size_bytes: 4 * 1024 ** 3,
				destroyed_without_copy: true,
			},
		],
	});

export const guestTargets = (
	over: Partial<GuestTargets> = {},
): GuestTargets => ({
	cluster: "pve-prod",
	nodes: [
		node("pve-01"),
		node("pve-02"),
		node("pve-03", { plugin_installed: false }),
		node("pve-04", { online: false }),
	],
	replace: [],
	snapshot_guest: { vmid: 200, name: "dns", uid: "uid-200" },
	source_node: "pve-01",
	storages: {
		[NODE_1]: [
			{ id: "local-lvm", shared: false, type: "lvmthin" },
			{ id: "local-zfs", shared: false, type: "zfspool" },
		],
		[NODE_2]: [{ id: "ceph", shared: true, type: "rbd" }],
		"h-pve-04": [{ id: "local-lvm", shared: false, type: "lvmthin" }],
	},
	...over,
});

export const replaceable = (): Partial<GuestTargets> => ({
	nodes: [node("pve-01", { in_place_allowed: true }), node("pve-02")],
	replace: [
		withDestroyed(200, "dns"),
		target(201, "mail", { running: true }),
		target(202, "ha-web", { ha_managed: true }),
	],
});

export type ContainerMock = {
	previews: (string | null)[];
	posts: GuestRestoreBody[];
};

/** What the server answers for a container snapshot; `post` can refuse. */
export function mockContainer(
	options: {
		facts?: ContainerFacts;
		factsFor?: (hostId: string | null) => ContainerFacts;
		targets?: GuestTargets;
		snapshot?: RestoreSnapshot;
		previewStatus?: number;
		targetsStatus?: number;
		post?: (body: GuestRestoreBody) => Response | undefined;
		stepUp?: boolean;
	} = {},
): ContainerMock {
	const mock: ContainerMock = {
		previews: [],
		posts: [],
	};
	const shown = options.snapshot ?? ctSnapshot;
	let steppedUp = false;
	server.use(
		http.get("/api/v1/restore/hosts", () => HttpResponse.json([restoreHost])),
		http.get("/api/v1/restore/snapshots", () =>
			HttpResponse.json({
				items: [shown],
				total: 1,
				next: null,
				latest_verified_id: shown.verified_by == null ? null : shown.id,
			}),
		),
		http.get("/api/v1/snapshots/:id", () => HttpResponse.json(shown)),
		http.get("/api/v1/snapshots/:id/entries", () =>
			HttpResponse.json({
				path: "",
				path_b64: "",
				entries: [],
				total: 0,
				offset: 0,
			}),
		),
		http.get("/api/v1/restore/snapshots/:id/container", ({ request }) => {
			const hostId = new URL(request.url).searchParams.get("host_id");
			mock.previews.push(hostId);
			if (options.previewStatus && hostId !== null) {
				return apiError(options.previewStatus, "guest_config_unreadable", "x");
			}
			return HttpResponse.json(
				options.factsFor?.(hostId) ?? options.facts ?? containerFacts(),
			);
		}),
		http.get("/api/v1/restore/guest-targets", () =>
			options.targetsStatus
				? apiError(options.targetsStatus, "unavailable", "down")
				: HttpResponse.json(options.targets ?? guestTargets()),
		),
		http.post("/api/v1/auth/step-up", () => {
			steppedUp = true;
			return new HttpResponse(null, { status: 204 });
		}),
		http.post("/api/v1/restores/guest", async ({ request }) => {
			const body = (await request.json()) as GuestRestoreBody;
			if (options.stepUp && !steppedUp) {
				return apiError(403, "step_up_required", "password required");
			}
			mock.posts.push(body);
			return (
				options.post?.(body) ??
				HttpResponse.json({ job_id: "job-1" }, { status: 201 })
			);
		}),
		http.get("/api/v1/jobs/:id", () =>
			apiError(404, "not_found", "no such job"),
		),
	);
	return mock;
}
