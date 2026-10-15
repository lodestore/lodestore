import { useQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { useListHosts } from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
import { AddHostDialog } from "@/components/hosts/add-host-dialog";
import { EditLabelsDialog } from "@/components/hosts/edit-labels-dialog";
import { HostsTable } from "@/components/hosts/hosts-table";
import { RevokeHostDialog } from "@/components/hosts/revoke-host-dialog";
import { StatTile } from "@/components/stat-tile";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";
import { infoQuery } from "@/lib/session";

export const Route = createFileRoute("/_app/hosts/")({
	validateSearch: (search: Record<string, unknown>): { add?: true } => ({
		add: [true, 1, "1", "true"].includes(search.add as never)
			? true
			: undefined,
	}),
	component: HostsPage,
});

const REFRESH_MS = 10_000;

function HostsPage() {
	const menuOrigin = useRef<string>(undefined);
	const { add } = Route.useSearch();
	const navigate = useNavigate();
	const [adding, setAdding] = useState(add === true);
	const [editing, setEditing] = useState<Host>();
	const [revoking, setRevoking] = useState<Host>();
	const hosts = useListHosts({
		query: { refetchInterval: REFRESH_MS, refetchIntervalInBackground: false },
	});
	const info = useQuery(infoQuery);

	useEffect(() => {
		if (!add) return;
		setAdding(true);
		void navigate({ to: "/hosts", search: {}, replace: true });
	}, [add, navigate]);

	const active = (hosts.data ?? []).filter((h) => h.status !== "revoked");
	const online = active.filter((h) => h.status === "online").length;

	function restoreFocus(event: Event) {
		const id = menuOrigin.current;
		if (id === undefined) return;
		event.preventDefault();
		document
			.querySelector<HTMLElement>(`[data-host-actions="${CSS.escape(id)}"]`)
			?.focus();
	}

	return (
		<div className="flex flex-col gap-6 pb-8">
			<div className="flex items-center justify-between gap-4">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.hosts.title}
				</h1>
				<Button onClick={() => setAdding(true)}>{copy.hosts.addHost}</Button>
			</div>
			{(hosts.isPending || active.length > 0) && (
				<div className="hidden grid-cols-3 gap-4 md:grid">
					<StatTile
						label={copy.hosts.tiles.total}
						value={active.length}
						isPending={hosts.isPending}
					/>
					<StatTile
						label={copy.hosts.tiles.online}
						value={online}
						isPending={hosts.isPending}
					/>
					<StatTile
						label={copy.hosts.tiles.offline}
						value={active.length - online}
						isPending={hosts.isPending}
					/>
				</div>
			)}
			<HostsTable
				hosts={hosts.data}
				isPending={hosts.isPending}
				isError={hosts.isError}
				onRetry={() => hosts.refetch()}
				serverVersion={info.data?.version}
				onAdd={() => setAdding(true)}
				onEditLabels={(host) => {
					menuOrigin.current = host.id;
					setEditing(host);
				}}
				onRevoke={(host) => {
					menuOrigin.current = host.id;
					setRevoking(host);
				}}
			/>
			<AddHostDialog open={adding} onOpenChange={setAdding} />
			<EditLabelsDialog
				host={editing}
				onClose={() => setEditing(undefined)}
				onCloseAutoFocus={restoreFocus}
			/>
			<RevokeHostDialog
				host={revoking}
				onClose={() => setRevoking(undefined)}
				onCloseAutoFocus={restoreFocus}
			/>
		</div>
	);
}
