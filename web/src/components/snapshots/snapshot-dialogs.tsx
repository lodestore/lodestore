import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert } from "lucide-react";
import { type FormEvent, useEffect, useId, useRef, useState } from "react";
import { toast } from "sonner";
import type { ApiError } from "@/api/fetcher";
import { useDeleteSnapshot, useSetSuspicious } from "@/api/gen/endpoints";
import type { SnapshotItem } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import { copy } from "@/copy";
import { shortId } from "@/lib/snapshot";

const text = copy.snapshots;
const REASON_MAX = 200;
const KEPT_TOAST_MS = 8000;

export type SnapshotAction = "mark" | "clear" | "delete";

type Props = {
	snapshot?: SnapshotItem;
	onClose: () => void;
	onCloseAutoFocus?: (event: Event) => void;
};

function useRefreshSnapshots() {
	const queryClient = useQueryClient();
	return () =>
		queryClient.invalidateQueries({
			predicate: ({ queryKey: [head] }) =>
				typeof head === "string" &&
				(head.includes("/snapshots") || head === "/api/v1/jobs"),
		});
}

function ReasonForm({
	snapshot,
	suspicious,
	mutation,
	onDone,
}: {
	snapshot: SnapshotItem;
	suspicious: boolean;
	mutation: ReturnType<typeof useSetSuspicious<ApiError>>;
	onDone: () => void;
}) {
	const id = useId();
	const refresh = useRefreshSnapshots();
	const [reason, setReason] = useState("");
	const [submitted, setSubmitted] = useState(false);
	const dialog = suspicious ? text.markDialog : text.clearDialog;
	const short = shortId(snapshot.id);
	const missing = submitted && reason.trim() === "";

	function submit(event: FormEvent) {
		event.preventDefault();
		setSubmitted(true);
		if (reason.trim() === "") return;
		mutation.mutate(
			{ id: snapshot.id, data: { suspicious, reason: reason.trim() } },
			{
				onSuccess: () => {
					toast.success(
						suspicious ? text.toast.marked(short) : text.toast.cleared(short),
					);
					void refresh();
					onDone();
				},
			},
		);
	}

	return (
		<form className="flex flex-col gap-6" onSubmit={submit}>
			<DialogHeader>
				<DialogTitle>{dialog.title(short)}</DialogTitle>
				<DialogDescription>{dialog.body}</DialogDescription>
			</DialogHeader>
			<div className="flex flex-col gap-2">
				<Label htmlFor={`${id}-reason`}>{text.reason.label}</Label>
				<Textarea
					id={`${id}-reason`}
					rows={3}
					autoFocus
					maxLength={REASON_MAX}
					placeholder={dialog.placeholder}
					value={reason}
					aria-invalid={missing}
					aria-describedby={missing ? `${id}-error` : undefined}
					onChange={(event) => setReason(event.target.value)}
				/>
				{missing && (
					<p
						id={`${id}-error`}
						className="flex items-start gap-1 text-sm text-destructive"
					>
						<CircleAlert aria-hidden className="mt-1 size-4 shrink-0" />
						{text.reason.required}
					</p>
				)}
			</div>
			{mutation.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{copy.errors.submit(dialog.action, mutation.error.message)}
					</AlertDescription>
				</Alert>
			)}
			<DialogFooter>
				<Button
					type="button"
					variant="outline"
					disabled={mutation.isPending}
					onClick={onDone}
				>
					{dialog.cancel}
				</Button>
				<Button type="submit" disabled={mutation.isPending}>
					{mutation.isPending && <Spinner />}
					{dialog.confirm}
				</Button>
			</DialogFooter>
		</form>
	);
}

function ReasonDialog({
	snapshot,
	suspicious,
	onClose,
	onCloseAutoFocus,
}: Props & { suspicious: boolean }) {
	const mutation = useSetSuspicious();

	function close() {
		mutation.reset();
		onClose();
	}

	return (
		<Dialog
			open={snapshot !== undefined}
			onOpenChange={(open) => {
				if (!open && !mutation.isPending) close();
			}}
		>
			<DialogContent
				className="sm:max-w-[560px]"
				onCloseAutoFocus={onCloseAutoFocus}
			>
				{snapshot && (
					<ReasonForm
						snapshot={snapshot}
						suspicious={suspicious}
						mutation={mutation}
						onDone={close}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}

export function MarkSuspiciousDialog(props: Props) {
	return <ReasonDialog {...props} suspicious />;
}

export function ClearSuspiciousDialog(props: Props) {
	return <ReasonDialog {...props} suspicious={false} />;
}

export function DeleteSnapshotDialog({
	snapshot,
	onClose,
	onCloseAutoFocus,
}: Props) {
	const refresh = useRefreshSnapshots();
	const remove = useDeleteSnapshot();
	const short = snapshot ? shortId(snapshot.id) : "";

	function close() {
		remove.reset();
		onClose();
	}

	function confirm() {
		if (!snapshot) return;
		remove.mutate(
			{ id: snapshot.id },
			{
				onSuccess: ({ storage_kept }) => {
					if (storage_kept) {
						toast.warning(text.toast.kept(short), { duration: KEPT_TOAST_MS });
					} else {
						toast.success(text.toast.deleted(short));
					}
					void refresh();
					close();
				},
			},
		);
	}

	return (
		<AlertDialog
			open={snapshot !== undefined}
			onOpenChange={(open) => {
				if (!open && !remove.isPending) close();
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle>{text.deleteDialog.title(short)}</AlertDialogTitle>
					<AlertDialogDescription>
						{text.deleteDialog.body}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{remove.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.errors.submit(
								text.deleteDialog.action,
								remove.error.message,
							)}
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={remove.isPending}>
						{text.deleteDialog.cancel}
					</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={remove.isPending}
						onClick={(event) => {
							event.preventDefault();
							confirm();
						}}
					>
						{remove.isPending && <Spinner />}
						{text.deleteDialog.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

export function SnapshotDialogs({
	target,
	onClose,
}: {
	target?: { snapshot: SnapshotItem; action: SnapshotAction };
	onClose: () => void;
}) {
	const last = useRef<string>(undefined);
	useEffect(() => {
		if (target) last.current = target.snapshot.id;
	}, [target]);
	const onCloseAutoFocus = (event: Event) => {
		const trigger = document.querySelector<HTMLElement>(
			`[data-snapshot-actions="${CSS.escape(last.current ?? "")}"]`,
		);
		if (!trigger) return;
		event.preventDefault();
		trigger.focus();
	};
	const props = (action: SnapshotAction) => ({
		snapshot: target?.action === action ? target.snapshot : undefined,
		onClose,
		onCloseAutoFocus,
	});
	return (
		<>
			<MarkSuspiciousDialog {...props("mark")} />
			<ClearSuspiciousDialog {...props("clear")} />
			<DeleteSnapshotDialog {...props("delete")} />
		</>
	);
}
