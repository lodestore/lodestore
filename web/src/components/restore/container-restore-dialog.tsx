import { useIsMutating, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
	CircleAlert,
	Lock,
	Network,
	Power,
	RefreshCw,
	TriangleAlert,
} from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import {
	getCreateGuestRestoreMutationKey,
	getListJobsQueryKey,
	useContainerPreview,
	useCreateGuestRestore,
	useGuestTargets,
} from "@/api/gen/endpoints";
import type {
	GuestRestoreBody,
	GuestTargets,
	ReplaceTarget,
	RestoreSnapshot,
	TargetNode,
} from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { DroppedKeys, KeyRows } from "@/components/restore/dropped-keys";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Dialog,
	DialogContent,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { formatBytes, formatDateTime } from "@/lib/format";
import { shortId } from "@/lib/snapshot";

const text = copy.restore.container;
const MIN_VMID = 100;
const MAX_VMID = 999_999_999;

const REFUSED_CODES = new Set([
	"restore_agent_outdated",
	"restore_expired",
	"restore_storage_invalid",
	"restore_other_guest_not_confirmed",
	"restore_unverified_in_place",
	"restore_unverified_not_confirmed",
	"guest_in_place_disabled",
	"guest_plugin_missing",
	"guest_ha_managed",
	"guest_running",
	"forbidden",
]);

const keyOf = (target: ReplaceTarget) => `${target.host_id}:${target.vmid}`;
const blocked = (target: ReplaceTarget) => target.running || target.ha_managed;
const mountPoint = (value: string) => /(?:^|,)mp=([^,]+)/.exec(value)?.[1];

function FieldMessage({ id, children }: { id: string; children: ReactNode }) {
	return (
		<p id={id} className="flex items-start gap-1 text-sm text-destructive">
			<CircleAlert aria-hidden className="mt-1 size-4 shrink-0" />
			{children}
		</p>
	);
}

function Section({ title, children }: { title: string; children: ReactNode }) {
	return (
		<div className="flex flex-col gap-1">
			{title && <h3 className="text-sm font-semibold">{title}</h3>}
			{children}
		</div>
	);
}

function nodeLabel(node: TargetNode) {
	if (!node.plugin_installed) return `${node.name} · ${text.pluginMissing}`;
	return node.online ? node.name : `${node.name} · ${text.offline}`;
}

function targetLabel(target: ReplaceTarget) {
	const state = target.ha_managed
		? text.haManaged
		: target.running
			? text.running
			: text.stopped;
	return text.targetLabel(target.vmid, target.name, target.node, state);
}

function Form({
	snapshot,
	targets,
	stamp,
	reload,
	onDone,
}: {
	snapshot: RestoreSnapshot;
	targets: GuestTargets;
	stamp: number;
	reload: () => void;
	onDone: () => void;
}) {
	const id = useId();
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const create = useCreateGuestRestore();
	const guest = targets.snapshot_guest;
	const unverified = snapshot.verified_by == null;
	const [acceptUnverified, setAcceptUnverified] = useState(false);

	const [wantReplace, setWantReplace] = useState(false);
	const nodes = targets.nodes.filter((node) => node.agent && node.host_id);
	const replaceOff = !nodes.some((node) => node.in_place_allowed);
	const replacing = wantReplace && !replaceOff && !unverified;

	const [pickedNode, setPickedNode] = useState<string>();
	const node =
		nodes.find((n) => n.host_id === pickedNode) ??
		nodes.find((n) => n.name === targets.source_node && n.plugin_installed) ??
		nodes.find((n) => n.plugin_installed);
	const storages = targets.storages[node?.host_id ?? ""] ?? [];
	const [pickedStorage, setPickedStorage] = useState<string>();
	const storage =
		storages.find((s) => s.id === pickedStorage)?.id ?? storages[0]?.id;
	const [edited, setVmid] = useState<string>();
	const vmidText =
		edited ?? (node?.next_vmid != null ? String(node.next_vmid) : "");
	const vmid = /^\d+$/.test(vmidText) ? Number(vmidText) : undefined;
	const vmidInvalid =
		vmidText !== "" &&
		(vmid === undefined || vmid < MIN_VMID || vmid > MAX_VMID);
	const [takenByServer, setTaken] = useState<number[]>([]);
	const taken =
		vmid !== undefined &&
		(targets.replace.some((target) => target.vmid === vmid) ||
			takenByServer.includes(vmid));

	const [pickedTarget, setPickedTarget] = useState<string>();
	const target =
		targets.replace.find((t) => keyOf(t) === pickedTarget) ??
		(pickedTarget === undefined
			? targets.replace.find((t) => !blocked(t) && !t.other_guest)
			: undefined);
	const [confirmedFor, setConfirmedFor] = useState<string>();
	const confirmKey = target && `${keyOf(target)}@${stamp}`;
	const confirmed = confirmKey !== undefined && confirmedFor === confirmKey;
	const targetStorages = targets.storages[target?.host_id ?? ""] ?? [];
	const rootStorage = target?.volumes
		.find((v) => v.key === "rootfs")
		?.value.split(":")[0];
	const replaceStorage = targetStorages.some((s) => s.id === rootStorage)
		? rootStorage
		: targetStorages[0]?.id;
	const destroyed = target?.volumes.filter((v) => v.destroyed_without_copy);

	const previewHost =
		(replacing ? target?.host_id : node?.host_id) ?? undefined;
	const preview = useContainerPreview(
		snapshot.id,
		{ host_id: previewHost },
		{ query: { enabled: previewHost !== undefined } },
	);
	const facts = preview.data;
	const previewWaiting = previewHost !== undefined && preview.isPending;

	const canStart = replacing
		? target !== undefined &&
			!blocked(target) &&
			confirmed &&
			replaceStorage !== undefined
		: node?.host_id != null &&
			storage !== undefined &&
			vmid !== undefined &&
			!vmidInvalid &&
			!taken &&
			(!unverified || acceptUnverified);
	const sending = create.isPending;

	function start(event: React.FormEvent) {
		event.preventDefault();
		if (!canStart || sending) return;
		const base = { snapshot_id: snapshot.id };
		let data: GuestRestoreBody;
		if (replacing && target && replaceStorage) {
			data = {
				...base,
				target_host_id: target.host_id,
				storage: replaceStorage,
				replace_vmid: target.vmid,
				confirm_other_guest: target.other_guest,
			};
		} else if (node?.host_id && storage && vmid !== undefined) {
			data = {
				...base,
				target_host_id: node.host_id,
				storage,
				vmid,
				accept_unverified: unverified,
			};
		} else {
			return;
		}
		create.mutate(
			{ data },
			{
				onSuccess: ({ job_id }) => {
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
					onDone();
					void navigate({ to: "/jobs/$id", params: { id: job_id } });
				},
				onError: (error) => {
					if (error.code === "restore_vmid_taken" && vmid !== undefined) {
						setTaken((list) => [...list, vmid]);
					}
				},
			},
		);
	}

	function retry() {
		create.reset();
		setConfirmedFor(undefined);
		reload();
	}

	const error = create.error;
	const submitAlert =
		error === null || error.code === "restore_vmid_taken"
			? undefined
			: error.code === "step_up_required"
				? undefined
				: error.code === "guest_facts_stale"
					? text.factsStale
					: error.code === "guest_target_unverifiable"
						? text.targetUnverifiable(target?.vmid ?? 0)
						: (REFUSED_CODES.has(error.code)
								? copy.errors.refused
								: copy.errors.submit)(text.failed, error.message);
	const retryable =
		error?.code === "guest_facts_stale" ||
		error?.code === "guest_target_unverifiable";

	const effects = [
		{ Icon: Power, label: text.effects.stopped },
		{ Icon: RefreshCw, label: text.effects.newMacs },
		...(facts?.unprivileged
			? [{ Icon: Lock, label: text.effects.unprivileged }]
			: []),
	];
	const nodeName = node?.name ?? nodes[0]?.name ?? "";
	const dropped = facts?.dropped ?? [];

	return (
		<form onSubmit={start} className="flex min-h-0 flex-col gap-6">
			<div className="-mx-6 -my-1 flex min-h-0 flex-col gap-6 overflow-y-auto px-6 py-1">
				<div className="flex flex-col gap-4">
					{unverified && !replacing && (
						<div className="flex items-start gap-2">
							<Checkbox
								id={`${id}-unverified`}
								checked={acceptUnverified}
								onCheckedChange={(value) => setAcceptUnverified(value === true)}
								className="mt-px"
							/>
							<Label
								htmlFor={`${id}-unverified`}
								className="leading-snug font-normal"
							>
								{text.unverifiedConfirm}
							</Label>
						</div>
					)}
					<div className="flex flex-col gap-2">
						<Label>{text.from}</Label>
						<p className="text-sm wrap-anywhere">
							{text.fromValue(
								targets.source_node ?? snapshot.hostname,
								shortId(snapshot.id),
								formatDateTime(snapshot.time),
								formatBytes(snapshot.stats.bytes_total),
							)}
						</p>
					</div>

					<div className="flex flex-col gap-2">
						<Label id={`${id}-as`}>{text.restoreAs}</Label>
						<RadioGroup
							aria-labelledby={`${id}-as`}
							value={replacing ? "replace" : "new"}
							onValueChange={(value) => {
								setWantReplace(value === "replace");
								setConfirmedFor(undefined);
								create.reset();
							}}
							className="gap-4"
						>
							<div className="flex items-center gap-2">
								<RadioGroupItem value="new" id={`${id}-new`} />
								<Label htmlFor={`${id}-new`}>{text.asNew}</Label>
							</div>
							<div className="flex items-start gap-2">
								<RadioGroupItem
									value="replace"
									id={`${id}-replace`}
									className="mt-px"
									disabled={replaceOff || unverified}
									aria-describedby={
										replaceOff || unverified ? `${id}-replace-help` : undefined
									}
								/>
								<div className="flex min-w-0 flex-col gap-1">
									<Label htmlFor={`${id}-replace`}>{text.asReplace}</Label>
									{(replaceOff || unverified) && (
										<p
											id={`${id}-replace-help`}
											className="text-sm text-muted-foreground"
										>
											{replaceOff ? (
												<>
													{text.replaceOffHead(nodeName)}{" "}
													<code className="font-mono">
														{text.replaceOffCommand}
													</code>{" "}
													{text.replaceOffTail}
												</>
											) : (
												text.replaceUnsigned
											)}
										</p>
									)}
								</div>
							</div>
						</RadioGroup>
					</div>

					{replacing ? (
						<div className="flex flex-col gap-2">
							<Label htmlFor={`${id}-target`}>{text.replaceTarget}</Label>
							<Select
								value={target ? keyOf(target) : ""}
								onValueChange={(value) => {
									setPickedTarget(value);
									setConfirmedFor(undefined);
									create.reset();
								}}
							>
								<SelectTrigger id={`${id}-target`} className="h-10 w-full">
									<SelectValue placeholder={text.choose} />
								</SelectTrigger>
								<SelectContent>
									{targets.replace.map((t) => (
										<SelectItem
											key={keyOf(t)}
											value={keyOf(t)}
											disabled={blocked(t)}
										>
											{targetLabel(t)}
										</SelectItem>
									))}
								</SelectContent>
							</Select>
						</div>
					) : (
						<div className="flex flex-col gap-2">
							<div className="grid grid-cols-1 gap-4 min-[680px]:grid-cols-[1fr_1fr_120px]">
								<div className="flex min-w-0 flex-col gap-2">
									<Label htmlFor={`${id}-node`}>{text.node}</Label>
									<Select
										value={node?.host_id ?? ""}
										onValueChange={(value) => {
											setPickedNode(value);
											setPickedStorage(undefined);
											create.reset();
										}}
									>
										<SelectTrigger id={`${id}-node`} className="h-10 w-full">
											<SelectValue />
										</SelectTrigger>
										<SelectContent>
											{nodes.map((n) => (
												<SelectItem
													key={n.host_id}
													value={n.host_id ?? ""}
													disabled={!n.plugin_installed}
												>
													{nodeLabel(n)}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</div>
								<div className="flex min-w-0 flex-col gap-2">
									<Label htmlFor={`${id}-storage`}>{text.storage}</Label>
									<Select
										value={storage ?? ""}
										onValueChange={setPickedStorage}
										disabled={storages.length === 0}
									>
										<SelectTrigger id={`${id}-storage`} className="h-10 w-full">
											<SelectValue placeholder={text.noStorage} />
										</SelectTrigger>
										<SelectContent>
											{storages.map((s) => (
												<SelectItem key={s.id} value={s.id}>
													{s.id}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</div>
								<div className="flex flex-col gap-2">
									<Label htmlFor={`${id}-vmid`}>{text.vmid}</Label>
									<Input
										id={`${id}-vmid`}
										inputMode="numeric"
										className="h-10 font-mono"
										value={vmidText}
										aria-invalid={taken || vmidInvalid}
										aria-describedby={
											taken || vmidInvalid ? `${id}-vmid-error` : undefined
										}
										onChange={(event) => {
											setVmid(event.target.value.trim());
											create.reset();
										}}
									/>
								</div>
							</div>
							{taken && vmid !== undefined && (
								<FieldMessage id={`${id}-vmid-error`}>
									{text.vmidTaken(vmid)}
								</FieldMessage>
							)}
							{vmidInvalid && !taken && (
								<FieldMessage id={`${id}-vmid-error`}>
									{text.vmidInvalid}
								</FieldMessage>
							)}
							{node?.next_vmid != null && (
								<p className="text-sm text-muted-foreground">
									{text.nextFree(node.next_vmid, targets.cluster, guest.name)}
								</p>
							)}
						</div>
					)}

					{replacing && target && (
						<>
							{target.other_guest && (
								<Alert role="status">
									<TriangleAlert aria-hidden className="text-warning" />
									<AlertDescription>
										{guest.uid == null || target.pve_uid == null
											? text.otherGuestUnknown
											: text.otherGuest(target.vmid)}
									</AlertDescription>
								</Alert>
							)}
							<Alert role="status">
								<TriangleAlert aria-hidden className="text-warning" />
								<AlertTitle>
									{text.replaceWarningTitle(target.vmid, target.name)}
								</AlertTitle>
								<AlertDescription className="flex flex-col gap-2">
									<p>{text.replaceWarningBody(target.vmid, dropped.length)}</p>
									{destroyed !== undefined && destroyed.length > 0 && (
										<div className="flex flex-col gap-1">
											<p>{text.replaceDestroyed}</p>
											<ul className="flex flex-col gap-1 font-mono text-sm">
												{destroyed.map((volume) => (
													<li key={volume.key}>
														<TextWithPaths
															text={text.replaceDestroyedRow(
																volume.key,
																mountPoint(volume.value),
																volume.size_bytes == null
																	? undefined
																	: formatBytes(volume.size_bytes),
															)}
														/>
													</li>
												))}
											</ul>
										</div>
									)}
								</AlertDescription>
							</Alert>
							<div className="flex items-start gap-2">
								<Checkbox
									id={`${id}-confirm`}
									checked={confirmed}
									onCheckedChange={(value) =>
										setConfirmedFor(value === true ? confirmKey : undefined)
									}
									className="mt-px"
								/>
								<Label
									htmlFor={`${id}-confirm`}
									className="leading-snug font-normal"
								>
									{text.replaceConfirm(target.vmid, target.name, target.node)}
								</Label>
							</div>
						</>
					)}

					{!replacing && (
						<Section title={text.effectsTitle}>
							<ul className="flex flex-col divide-y">
								{effects.map(({ Icon, label }) => (
									<li
										key={label}
										className="flex items-center gap-2 py-2 text-sm"
									>
										<Icon
											aria-hidden
											className="size-4 shrink-0 text-muted-foreground"
										/>
										{label}
									</li>
								))}
							</ul>
						</Section>
					)}

					{previewWaiting ? (
						<div aria-hidden className="flex flex-col gap-2">
							<Skeleton className="h-4 w-48" />
							<Skeleton className="h-4 w-full" />
							<Skeleton className="h-4 w-full" />
						</div>
					) : preview.isError ? (
						<p className="text-sm text-muted-foreground">
							{text.previewFailed}
						</p>
					) : facts ? (
						<>
							<Section
								title={dropped.length > 0 ? text.dropped(dropped.length) : ""}
							>
								<DroppedKeys dropped={dropped} collapsed={replacing} />
							</Section>
							{!replacing && facts.network.length > 0 && (
								<Section title={text.network}>
									<KeyRows rows={facts.network} icon={Network} />
									{!facts.bridges_checked && (
										<p className="text-sm text-muted-foreground">
											{text.networkNotChecked}
										</p>
									)}
								</Section>
							)}
							<p className="text-sm text-muted-foreground">
								{text.agentFilters(
									target && replacing ? target.node : nodeName,
								)}
							</p>
						</>
					) : null}
				</div>

				{submitAlert && (
					<Alert variant="destructive" role="alert">
						<AlertDescription className="flex flex-col items-start gap-2">
							<span>{submitAlert}</span>
							{retryable && (
								<Button
									type="button"
									variant="outline"
									size="sm"
									onClick={retry}
								>
									{text.retry}
								</Button>
							)}
						</AlertDescription>
					</Alert>
				)}
			</div>

			<DialogFooter>
				<Button
					type="button"
					variant="outline"
					disabled={sending}
					onClick={onDone}
				>
					{text.dismiss}
				</Button>
				<Button
					type="submit"
					variant={replacing ? "destructive" : "default"}
					disabled={!canStart || sending}
				>
					{sending && <Spinner />}
					{replacing && target
						? text.replaceButton(target.vmid)
						: text.confirmNew(vmid ?? "")}
				</Button>
			</DialogFooter>
		</form>
	);
}

function Loader({
	snapshot,
	onDone,
}: {
	snapshot: RestoreSnapshot;
	onDone: () => void;
}) {
	const targets = useGuestTargets({ snapshot_id: snapshot.id });
	if (targets.isError) {
		return (
			<div className="flex flex-col gap-6">
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex flex-col items-start gap-2">
						<span>{text.nodesFailed}</span>
						<Button
							type="button"
							variant="outline"
							size="sm"
							onClick={() => void targets.refetch()}
						>
							{text.retry}
						</Button>
					</AlertDescription>
				</Alert>
				<DialogFooter>
					<Button type="button" variant="outline" onClick={onDone}>
						{text.dismiss}
					</Button>
				</DialogFooter>
			</div>
		);
	}
	if (targets.data === undefined) {
		return (
			<div className="flex flex-col gap-6">
				<div aria-hidden className="flex flex-col gap-4">
					<Skeleton className="h-10 w-full" />
					<Skeleton className="h-10 w-full" />
					<Skeleton className="h-24 w-full" />
				</div>
				<DialogFooter>
					<Button type="button" variant="outline" onClick={onDone}>
						{text.dismiss}
					</Button>
					<Button type="submit" disabled>
						{text.confirmNew("")}
					</Button>
				</DialogFooter>
			</div>
		);
	}
	return (
		<Form
			snapshot={snapshot}
			targets={targets.data}
			stamp={targets.dataUpdatedAt}
			reload={() => void targets.refetch()}
			onDone={onDone}
		/>
	);
}

function ContainerTitle({
	snapshot,
}: {
	snapshot: RestoreSnapshot | undefined;
}) {
	const facts = useContainerPreview(snapshot?.id ?? "", undefined, {
		query: { enabled: snapshot !== undefined },
	}).data;
	return text.title(facts?.vmid, facts?.name);
}

/** The step-up for a replace is asked by the global StepUpDialog when the request answers `step_up_required`. */
export function ContainerRestoreDialog({
	open,
	onOpenChange,
	snapshot,
	onCloseAutoFocus,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	snapshot: RestoreSnapshot | undefined;
	onCloseAutoFocus: (event: Event) => void;
}) {
	const busy =
		useIsMutating({ mutationKey: getCreateGuestRestoreMutationKey() }) > 0;
	return (
		<Dialog
			open={open}
			onOpenChange={(next) => (next || !busy) && onOpenChange(next)}
		>
			<DialogContent
				onCloseAutoFocus={onCloseAutoFocus}
				aria-describedby={undefined}
				className="flex max-h-[calc(100dvh-2rem)] flex-col sm:max-w-140"
			>
				<DialogHeader>
					<DialogTitle className="text-2xl">
						<ContainerTitle snapshot={snapshot} />
					</DialogTitle>
				</DialogHeader>
				{snapshot && (
					<Loader snapshot={snapshot} onDone={() => onOpenChange(false)} />
				)}
			</DialogContent>
		</Dialog>
	);
}
