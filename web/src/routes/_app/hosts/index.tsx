import { useQuery } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { useListHosts } from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
import { AddHostDialog } from "@/components/hosts/add-host-dialog";
import { OutdatedBanner } from "@/components/hosts/agent-update/outdated-banner";
import { RolloutCard } from "@/components/hosts/agent-update/rollout-card";
import { UpdateConfirmDialog } from "@/components/hosts/agent-update/update-confirm-dialog";
import { UpdateSelectionBar } from "@/components/hosts/agent-update/update-selection-bar";
import { useRollout } from "@/components/hosts/agent-update/use-rollout";
import { EditLabelsDialog } from "@/components/hosts/edit-labels-dialog";
import { HostsTable } from "@/components/hosts/hosts-table";
import { RevokeHostDialog } from "@/components/hosts/revoke-host-dialog";
import { RetryAlert } from "@/components/restore/restore-states";
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
	const rollout = useRollout();
	const [selected, setSelected] = useState<Set<string>>(new Set());
	const [confirming, setConfirming] = useState<"outdated" | "selected">();
	const rolloutState = rollout.status?.rollout?.state;
	const selectable =
		rollout.status?.signed === true &&
		rolloutState !== "running" &&
		rolloutState !== "paused";
	const select = (ids: string[], checked: boolean) =>
		setSelected((prev) => {
			const next = new Set(prev);
			for (const id of ids) {
				if (checked) next.add(id);
				else next.delete(id);
			}
			return next;
		});

	useEffect(() => {
		if (rolloutState === "running" || rolloutState === "paused") {
			setConfirming(undefined);
		}
	}, [rolloutState]);

	useEffect(() => {
		if (!add) return;
		setAdding(true);
		void navigate({ to: "/hosts", search: {}, replace: true });
	}, [add, navigate]);

	const active = (hosts.data ?? []).filter((h) => h.status !== "revoked");
	const online = active.filter((h) => h.status === "online").length;
	const activeIds = new Set(active.map((h) => h.id));
	const updatableIds = (rollout.status?.hosts ?? [])
		.filter((h) => h.outdated && h.updatable && activeIds.has(h.id))
		.map((h) => h.id);
	const pickedIds = updatableIds.filter((id) => selected.has(id));
	const targetIds = confirming === "selected" ? pickedIds : updatableIds;
	const offlineTargets = active.filter(
		(h) => h.status === "offline" && targetIds.includes(h.id),
	).length;
	const serverVersion = info.data?.version;
	const differs =
		serverVersion !== undefined &&
		active.some((h) => h.version !== serverVersion);

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
			{rollout.status?.rollout ? (
				<RolloutCard
					rollout={rollout.status.rollout}
					pending={rollout.acting}
					onPause={rollout.pause}
					onResume={rollout.resume}
					onCancel={rollout.cancel}
					onDismiss={rollout.dismiss}
				/>
			) : (
				rollout.isError &&
				!rollout.status && (
					<RetryAlert onRetry={() => rollout.refetch()}>
						{copy.agentUpdate.loadFailed}
					</RetryAlert>
				)
			)}
			<OutdatedBanner
				status={rollout.status}
				count={updatableIds.length}
				differs={differs}
				onUpdate={() => setConfirming("outdated")}
			/>
			{selectable && (
				<UpdateSelectionBar
					count={pickedIds.length}
					onUpdate={() => setConfirming("selected")}
					onClear={() => setSelected(new Set())}
				/>
			)}
			<HostsTable
				hosts={hosts.data}
				isPending={hosts.isPending}
				isError={hosts.isError}
				onRetry={() => hosts.refetch()}
				updates={rollout.byHost}
				status={rollout.status}
				selectable={selectable}
				selected={selected}
				onSelect={(id, checked) => select([id], checked)}
				onSelectAll={select}
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
			{rollout.status && (
				<UpdateConfirmDialog
					open={confirming !== undefined}
					onOpenChange={(open) => !open && setConfirming(undefined)}
					status={rollout.status}
					count={targetIds.length}
					offline={offlineTargets}
					pending={rollout.starting}
					onConfirm={() =>
						rollout.start(
							confirming === "selected"
								? { hosts: targetIds }
								: { outdated: true },
							() => {
								setConfirming(undefined);
								setSelected(new Set());
							},
						)
					}
				/>
			)}
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
