import { screen } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import type {
	AlertThresholds,
	ChannelDto,
	ChannelInput,
	ChannelTestInput,
} from "@/api/gen/model";
import { renderApp, useSignedIn } from "./app";
import { server } from "./server";

export const ALL_TYPES: ChannelDto["alert_types"] = [
	"backup_failed",
	"no_recent_backup",
	"agent_offline",
	"check_failed",
	"certificate_expiring",
	"agent_update_failed",
];

export const channel = (
	name: string,
	over: Partial<ChannelDto> = {},
): ChannelDto => ({
	id: `id-${name}`,
	name,
	type: "webhook",
	enabled: true,
	alert_types: ALL_TYPES,
	source: "ui",
	settings: { url: "https://alerts.example.com/hook" },
	secret_set: false,
	last_delivery: null,
	...over,
});

export const thresholds: AlertThresholds = {
	offline_after: "15m",
	offline_floor: "5m",
	cert_days: 14,
	cert_days_max: 60,
};

export function mockChannels(
	initial: ChannelDto[],
	options: { testResult?: { ok: boolean; message: string } } = {},
) {
	let items = initial;
	let limits = thresholds;
	const calls = {
		created: [] as ChannelInput[],
		updated: [] as { id: string; body: ChannelInput }[],
		deleted: [] as string[],
		tested: [] as ChannelTestInput[],
		thresholds: [] as unknown[],
	};
	const base = "/api/v1/notifications";
	server.use(
		http.get(`${base}/channels`, () => HttpResponse.json(items)),
		http.post(`${base}/channels`, async ({ request }) => {
			const body = (await request.json()) as ChannelInput;
			calls.created.push(body);
			const created = channel(body.name, {
				type: body.type,
				enabled: body.enabled ?? true,
				alert_types: body.alert_types ?? ALL_TYPES,
				settings: body.settings ?? {},
				secret_set: Boolean(body.secret),
			});
			items = [...items, created];
			return HttpResponse.json(created, { status: 201 });
		}),
		http.post(`${base}/channels/test`, async ({ request }) => {
			const body = (await request.json()) as ChannelTestInput;
			calls.tested.push(body);
			return HttpResponse.json(
				options.testResult ?? { ok: true, message: "sent" },
			);
		}),
		http.get(`${base}/channels/:id`, ({ params }) => {
			const found = items.find((c) => c.id === params.id);
			return found
				? HttpResponse.json(found)
				: HttpResponse.json(
						{ code: "not_found", message: "channel not found" },
						{ status: 404 },
					);
		}),
		http.put(`${base}/channels/:id`, async ({ request, params }) => {
			const body = (await request.json()) as ChannelInput;
			calls.updated.push({ id: String(params.id), body });
			const current = items.find((c) => c.id === params.id);
			const next = channel(body.name, {
				id: String(params.id),
				type: body.type,
				enabled: body.enabled ?? true,
				alert_types: body.alert_types ?? ALL_TYPES,
				settings: body.settings ?? {},
				secret_set: body.secret ? true : (current?.secret_set ?? false),
			});
			items = items.map((c) => (c.id === params.id ? next : c));
			return HttpResponse.json(next);
		}),
		http.delete(`${base}/channels/:id`, ({ params }) => {
			calls.deleted.push(String(params.id));
			items = items.filter((c) => c.id !== params.id);
			return new HttpResponse(null, { status: 204 });
		}),
		http.get(`${base}/thresholds`, () => HttpResponse.json(limits)),
		http.put(`${base}/thresholds`, async ({ request }) => {
			const body = (await request.json()) as Partial<AlertThresholds>;
			calls.thresholds.push(body);
			limits = {
				...limits,
				offline_after: body.offline_after ?? limits.offline_after,
				cert_days: body.cert_days ?? limits.cert_days,
			};
			return HttpResponse.json(limits);
		}),
	);
	return calls;
}

export async function openNotifications(
	path = "/settings/notifications",
	heading = "Notifications",
) {
	useSignedIn();
	const app = renderApp(path);
	await screen.findByRole("heading", { name: heading, level: 1, hidden: true });
	return app;
}
