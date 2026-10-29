import { HttpResponse, http } from "msw";
import { setupServer } from "msw/node";

export const emptyFleet = {
	generated_at: 0,
	totals: {
		hosts: 0,
		healthy: 0,
		overdue: 0,
		failed: 0,
		offline: 0,
		not_protected: 0,
	},
	storage: { repositories: 0, counted: 0 },
	hosts: [],
};

export const server = setupServer(
	http.get("/api/v1/catalog", () => HttpResponse.json([])),
	http.get("/api/v1/fleet", () =>
		HttpResponse.json({ ...emptyFleet, generated_at: Date.now() }),
	),
	http.get("/api/v1/fleet/hosts/:id", () =>
		HttpResponse.json(
			{ code: "not_found", message: "host not found" },
			{ status: 404 },
		),
	),
	http.get("/api/v1/alerts", () => HttpResponse.json({ alerts: [], total: 0 })),
	http.get("/api/v1/alerts/count", () =>
		HttpResponse.json({ active: 0, unsilenced: 0 }),
	),
	http.get("/api/v1/repositories/:id/checks", ({ params }) =>
		HttpResponse.json({
			repository_id: params.id,
			levels: [],
			usage: null,
			coverage: null,
			blocked: null,
		}),
	),
	http.get("/api/v1/notifications/channels", () => HttpResponse.json([])),
	http.get("/api/v1/account", () =>
		HttpResponse.json({ username: "admin", email: null }),
	),
);
