import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { useRef, useState } from "react";
import { useListHosts } from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
import { AddHostDialog } from "@/components/hosts/add-host-dialog";
import { EditLabelsDialog } from "@/components/hosts/edit-labels-dialog";
import { HostDetailsSheet } from "@/components/hosts/host-details-sheet";
import { HostsTable } from "@/components/hosts/hosts-table";
import { RevokeHostDialog } from "@/components/hosts/revoke-host-dialog";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";
import { infoQuery } from "@/lib/session";

export const Route = createFileRoute("/_app/hosts")({
	component: HostsPage,
});

const REFRESH_MS = 10_000;

function HostsPage() {
	const menuOrigin = useRef<string>(undefined);
	const [adding, setAdding] = useState(false);
	const [viewing, setViewing] = useState<Host>();
	const [editing, setEditing] = useState<Host>();
	const [revoking, setRevoking] = useState<Host>();
	const hosts = useListHosts({
		query: { refetchInterval: REFRESH_MS, refetchIntervalInBackground: false },
	});
	const info = useQuery(infoQuery);

	function restoreFocus(event: Event) {
		const id = menuOrigin.current;
		if (id === undefined) return;
		event.preventDefault();
		document
			.querySelector<HTMLElement>(`[data-host-actions="${CSS.escape(id)}"]`)
			?.focus();
	}

	return (
		<div className="flex flex-col gap-4 pb-8">
			<div className="flex items-center justify-between gap-4">
				<h1 className="text-2xl font-semibold">{copy.hosts.title}</h1>
				<Button onClick={() => setAdding(true)}>{copy.hosts.addHost}</Button>
			</div>
			<HostsTable
				hosts={hosts.data}
				isPending={hosts.isPending}
				isError={hosts.isError}
				onRetry={() => hosts.refetch()}
				serverVersion={info.data?.version}
				onAdd={() => setAdding(true)}
				onView={(host, via) => {
					menuOrigin.current = via === "menu" ? host.id : undefined;
					setViewing(host);
				}}
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
			<HostDetailsSheet
				host={viewing}
				onClose={() => setViewing(undefined)}
				onEditLabels={(host) => {
					menuOrigin.current = undefined;
					setEditing(host);
				}}
				onRevoke={(host) => {
					menuOrigin.current = undefined;
					setRevoking(host);
				}}
				onCloseAutoFocus={restoreFocus}
			/>
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
