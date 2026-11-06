import { useIsMutating, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
	ChevronDown,
	CircleAlert,
	Info,
	ShieldAlert,
	TriangleAlert,
} from "lucide-react";
import { useId, useState } from "react";
import { toast } from "sonner";
import {
	getCreateRestoreMutationKey,
	getListJobsQueryKey,
	summarizeSelection,
	useCreateRestore,
	useListRestoreHosts,
} from "@/api/gen/endpoints";
import type { ConflictPolicyDto, RestoreSnapshot } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { HostPicker, isRestoreTarget } from "@/components/restore/host-picker";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
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
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";
import { formatBytes, formatDateTime } from "@/lib/format";
import { displayName } from "@/lib/names";
import { shortId } from "@/lib/snapshot";

const TOAST_MS = 8000;
const text = copy.restore.restoreDialog;
const CONFLICTS = Object.keys(text.conflictOptions) as ConflictPolicyDto[];

const DEFAULT_ROOT = "/var/lib/lodestore/restore";

const pad = (n: number) => String(n).padStart(2, "0");

/** `YYYY-MM-DD` in local time, the day the timeline groups the snapshot under. */
export function localDate(ms: number) {
	const date = new Date(ms);
	return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** Same rule as the server: no `..`, empty and `.` parts dropped, never `/`. */
export function checkFolder(path: string): { error?: string; folder: string } {
	const parts = path.split("/").filter((part) => part && part !== ".");
	const folder = `/${parts.join("/")}`;
	if (!path.startsWith("/")) return { error: text.pathInvalid, folder };
	if (parts.includes("..")) return { error: text.pathDotDot, folder };
	return { error: folder === "/" ? text.pathRoot : undefined, folder };
}

function FieldMessage({ id, children }: { id: string; children: string }) {
	return (
		<p id={id} className="flex items-start gap-1 text-sm text-destructive">
			<CircleAlert aria-hidden className="mt-1 size-4 shrink-0" />
			{children}
		</p>
	);
}

function RestoreForm({
	snapshot,
	items,
	onDone,
}: {
	snapshot: RestoreSnapshot;
	items: string[];
	onDone: () => void;
}) {
	const id = useId();
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const hosts = useListRestoreHosts().data ?? [];
	const source = hosts.find((host) => host.key === snapshot.host_key);
	const sourceName = displayName(source?.hostname ?? snapshot.hostname);

	const [picked, setPicked] = useState<string>();
	const targetKey =
		picked ?? (source && isRestoreTarget(source) ? source.key : undefined);
	const target = hosts.find((host) => host.key === targetKey);
	const targetName = displayName(target?.hostname ?? "");

	const unverified = snapshot.verified_by == null;
	const [acceptUnverified, setAcceptUnverified] = useState(false);
	const [inPlace, setInPlace] = useState(false);
	const [confirmedFor, setConfirmedFor] = useState<string>();
	const confirmed =
		inPlace && targetKey !== undefined && confirmedFor === targetKey;
	const [edited, setPath] = useState<string>();
	const path =
		edited ??
		`${target?.restore_roots[0] ?? DEFAULT_ROOT}/${localDate(snapshot.time)}-${shortId(snapshot.id)}`;
	const [conflict, setConflict] =
		useState<ConflictPolicyDto>("overwrite_changed");
	const policy: ConflictPolicyDto =
		inPlace && conflict === "stop_if_not_empty"
			? "overwrite_changed"
			: conflict;
	const policies = inPlace
		? CONFLICTS.filter((option) => option !== "stop_if_not_empty")
		: CONFLICTS;
	const [owners, setOwners] = useState(true);

	const counts = useQuery({
		queryKey: ["selection-summary", snapshot.id, items],
		queryFn: ({ signal }) =>
			summarizeSelection(snapshot.id, { items }, undefined, signal),
		retry: false,
		staleTime: 0,
	});
	const restore = useCreateRestore();

	const { error: folderError, folder } = checkFolder(path);
	const pathError = inPlace ? undefined : folderError;
	const hostError = target === undefined;
	const canStart =
		pathError === undefined &&
		!hostError &&
		(!inPlace || confirmed) &&
		(!unverified || acceptUnverified) &&
		!restore.isPending;
	const otherHost = target !== undefined && target.key !== snapshot.host_key;
	const otherRepository =
		otherHost &&
		target.repository_id != null &&
		target.repository_id !== snapshot.repository_id;

	function start(event: React.FormEvent) {
		event.preventDefault();
		if (!canStart || !target?.host_id) return;
		restore.mutate(
			{
				data: {
					snapshot_id: snapshot.id,
					items,
					target_host_id: target.host_id,
					destination: inPlace ? "/" : folder,
					in_place: inPlace,
					conflict: policy,
					restore_owners: owners,
					accept_unverified: unverified,
				},
			},
			{
				onSuccess: ({ job_id }) => {
					toast.success(text.queued(target.hostname), {
						duration: TOAST_MS,
						action: {
							label: copy.restore.viewJob,
							onClick: () =>
								void navigate({ to: "/jobs/$id", params: { id: job_id } }),
						},
					});
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
					onDone();
				},
			},
		);
	}

	return (
		<form onSubmit={start} className="flex min-h-0 flex-col gap-6">
			<div className="-mx-6 -my-1 flex min-h-0 flex-col gap-6 overflow-y-auto px-6 py-1">
				<div className="flex flex-col gap-4">
					{snapshot.suspicious && (
						<Alert role="status">
							<ShieldAlert aria-hidden className="text-warning" />
							<AlertDescription>{text.suspiciousWarning}</AlertDescription>
						</Alert>
					)}
					{unverified && (
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
						<p className="truncate text-sm">
							{text.fromValue(
								sourceName,
								shortId(snapshot.id),
								formatDateTime(snapshot.time),
							)}
						</p>
						<p className="text-sm text-muted-foreground">
							{counts.isPending
								? text.counting
								: counts.isError
									? text.countFailed
									: text.counted(
											counts.data.files,
											formatBytes(counts.data.bytes),
										)}
						</p>
					</div>

					<div className="flex flex-col gap-2">
						<Label>{text.target}</Label>
						<HostPicker
							mode="target"
							value={targetKey}
							sourceKey={snapshot.host_key}
							onChange={setPicked}
							invalid={hostError}
							describedBy={hostError ? `${id}-host` : undefined}
						/>
						{hostError && (
							<FieldMessage id={`${id}-host`}>{text.hostMissing}</FieldMessage>
						)}
						{otherHost && (
							<Alert role="status">
								<Info aria-hidden />
								<AlertDescription>
									{text.otherHost(targetName, sourceName)}
									{otherRepository && ` ${text.otherRepository}`}
								</AlertDescription>
							</Alert>
						)}
						{target?.status === "offline" && (
							<Alert role="status">
								<Info aria-hidden />
								<AlertDescription>{text.offline(targetName)}</AlertDescription>
							</Alert>
						)}
					</div>

					<div className="flex flex-col gap-2">
						<Label id={`${id}-destination`}>{text.destination}</Label>
						<RadioGroup
							aria-labelledby={`${id}-destination`}
							value={inPlace ? "original" : "new"}
							onValueChange={(value) => {
								setInPlace(value === "original");
								setConfirmedFor(undefined);
							}}
							className="gap-4"
						>
							<div className="flex flex-col gap-2">
								<div className="flex items-start gap-2">
									<RadioGroupItem
										value="new"
										id={`${id}-new`}
										aria-describedby={`${id}-new-help`}
									/>
									<div className="flex min-w-0 flex-col gap-1">
										<Label htmlFor={`${id}-new`}>{text.newFolder}</Label>
										<p
											id={`${id}-new-help`}
											className="text-sm text-muted-foreground"
										>
											{text.newFolderHelp}
										</p>
									</div>
								</div>
								{!inPlace && (
									<div className="flex flex-col gap-1 pl-6">
										<Input
											aria-label={text.folderOn(targetName || sourceName)}
											className="font-mono text-sm"
											value={path}
											aria-invalid={pathError !== undefined}
											aria-describedby={pathError ? `${id}-path` : undefined}
											onChange={(event) => setPath(event.target.value)}
										/>
										{pathError && (
											<FieldMessage id={`${id}-path`}>{pathError}</FieldMessage>
										)}
									</div>
								)}
							</div>
							<div className="flex flex-col gap-2">
								<div className="flex items-start gap-2">
									<RadioGroupItem
										value="original"
										id={`${id}-original`}
										disabled={unverified}
										aria-describedby={
											unverified ? `${id}-original-help` : undefined
										}
									/>
									<div className="flex min-w-0 flex-col gap-1">
										<Label htmlFor={`${id}-original`}>{text.original}</Label>
										{unverified && (
											<p
												id={`${id}-original-help`}
												className="text-sm text-muted-foreground"
											>
												{text.unverifiedInPlaceDisabled}
											</p>
										)}
									</div>
								</div>
								{inPlace && (
									<div className="flex flex-col gap-2 pl-6">
										<Alert role="status">
											<TriangleAlert aria-hidden className="text-warning" />
											<AlertDescription>
												{text.originalWarning}
											</AlertDescription>
										</Alert>
										<div className="flex items-start gap-2">
											<Checkbox
												id={`${id}-confirm`}
												checked={confirmed}
												onCheckedChange={(value) =>
													setConfirmedFor(
														value === true ? targetKey : undefined,
													)
												}
												className="mt-px"
											/>
											<Label
												htmlFor={`${id}-confirm`}
												className="leading-snug font-normal"
											>
												{text.originalConfirm(targetName)}
											</Label>
										</div>
									</div>
								)}
							</div>
						</RadioGroup>
					</div>

					<Collapsible className="flex flex-col gap-4">
						<CollapsibleTrigger asChild>
							<Button
								type="button"
								variant="ghost"
								className="group/options h-auto max-w-full self-start px-0 py-1 text-sm hover:bg-transparent aria-expanded:bg-transparent dark:hover:bg-transparent"
							>
								<ChevronDown
									aria-hidden
									className="shrink-0 transition-transform group-data-[state=closed]/options:-rotate-90"
								/>
								<span className="font-semibold">{text.options}</span>
								<span className="min-w-0 truncate font-normal text-muted-foreground">
									{text.optionsValue(
										text.conflictOptions[policy],
										owners ? text.ownersShort.on : text.ownersShort.off,
									)}
								</span>
							</Button>
						</CollapsibleTrigger>
						<CollapsibleContent className="flex flex-col gap-4">
							<div className="flex flex-col gap-2">
								<Label htmlFor={`${id}-conflict`}>{text.conflict}</Label>
								<Select
									value={policy}
									onValueChange={(value) =>
										setConflict(value as ConflictPolicyDto)
									}
								>
									<SelectTrigger id={`${id}-conflict`} className="h-10 w-full">
										<SelectValue />
									</SelectTrigger>
									<SelectContent>
										{policies.map((option) => (
											<SelectItem key={option} value={option}>
												{text.conflictOptions[option]}
											</SelectItem>
										))}
									</SelectContent>
								</Select>
							</div>
							<div className="flex items-center gap-2">
								<Switch
									id={`${id}-owners`}
									checked={owners}
									onCheckedChange={setOwners}
								/>
								<Label htmlFor={`${id}-owners`}>{text.owners}</Label>
							</div>
						</CollapsibleContent>
					</Collapsible>
				</div>

				{restore.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							<TextWithPaths
								text={copy.errors.submit(text.failed, restore.error.message)}
							/>
						</AlertDescription>
					</Alert>
				)}
			</div>

			<DialogFooter>
				<Button
					type="button"
					variant="outline"
					disabled={restore.isPending}
					onClick={onDone}
				>
					{text.dismiss}
				</Button>
				<Button
					type="submit"
					variant={inPlace ? "destructive" : "default"}
					disabled={!canStart}
				>
					{restore.isPending && <Spinner />}
					{inPlace ? text.confirmInPlace : text.confirm}
				</Button>
			</DialogFooter>
		</form>
	);
}

export function RestoreDialog({
	open,
	onOpenChange,
	snapshot,
	items,
	onCloseAutoFocus,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	snapshot: RestoreSnapshot | undefined;
	items: string[];
	onCloseAutoFocus: (event: Event) => void;
}) {
	const busy =
		useIsMutating({ mutationKey: getCreateRestoreMutationKey() }) > 0;
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
						{text.title(items.length)}
					</DialogTitle>
				</DialogHeader>
				{snapshot && (
					<RestoreForm
						snapshot={snapshot}
						items={items}
						onDone={() => onOpenChange(false)}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}
