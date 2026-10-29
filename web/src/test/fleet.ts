import { HttpResponse, http } from "msw";
import type {
	AlertDto,
	AlertType,
	FleetHost,
	FleetOverview,
	HostState,
} from "@/api/gen/model";
import { emptyFleet, server } from "./server";

export const GIB = 1024 ** 3;

export const fleetHost = (
	state: HostState,
	over: Partial<FleetHost> = {},
): FleetHost => ({
	id: `host-${state}`,
	hostname: `${state}.example.com`,
	labels: [],
	platform: "linux/x86_64",
	state,
	agent_online: state !== "offline",
	silenced: false,
	has_policy: state !== "not_protected",
	recent_runs: [],
	...over,
});

export const fleetStorage = {
	repositories: 2,
	counted: 2,
	counted_at: Date.now() - 2 * 3_600_000,
	logical_bytes: 34 * GIB,
	physical_bytes: 10 * GIB,
	dedup_ratio: 3.4,
};

export const fleetOverview = (
	counts: Partial<Record<HostState, number>> = {},
	over: Partial<FleetOverview> = {},
): FleetOverview => {
	const totals = { ...emptyFleet.totals };
	const hosts: FleetHost[] = [];
	for (const [state, n] of Object.entries(counts) as [HostState, number][]) {
		totals[state] = n;
		totals.hosts += n;
		for (let i = 0; i < n; i += 1) {
			hosts.push(fleetHost(state, { id: `host-${state}-${i}` }));
		}
	}
	return {
		generated_at: Date.now(),
		totals,
		storage: fleetStorage,
		hosts,
		...over,
	};
};

export const useFleet = (overview: FleetOverview) =>
	server.use(http.get("/api/v1/fleet", () => HttpResponse.json(overview)));

export const alertFixture = (
	id: string,
	over: Partial<AlertDto> = {},
): AlertDto => ({
	id,
	type: "backup_failed",
	severity: "critical",
	state: "firing",
	silenced: false,
	started_at: Date.now() - 3_600_000,
	title: "",
	detail: "",
	facts: { error: "disk full" },
	host: { id: "host-1", hostname: "web-1" },
	policy: "daily",
	...over,
});

export const hostAlert = (
	host: FleetHost,
	type: AlertType,
	over: Partial<AlertDto> = {},
): AlertDto =>
	alertFixture(`${host.id}-${type}`, {
		type,
		severity: type === "backup_failed" ? "critical" : "warning",
		host: { id: host.id, hostname: host.hostname },
		...over,
	});

export const useAlerts = (alerts: AlertDto[], total = alerts.length) =>
	server.use(
		http.get("/api/v1/alerts", () => HttpResponse.json({ alerts, total })),
	);

export const useAlertCount = (unsilenced: number, active = unsilenced) =>
	server.use(
		http.get("/api/v1/alerts/count", () =>
			HttpResponse.json({ active, unsilenced }),
		),
	);

export const fleetOf = (hosts: FleetHost[]): FleetOverview => {
	const totals = { ...emptyFleet.totals, hosts: hosts.length };
	for (const host of hosts) totals[host.state] += 1;
	return {
		generated_at: Date.now(),
		totals,
		storage: fleetStorage,
		hosts,
	};
};
