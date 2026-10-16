import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import {
	getGetJobQueryKey,
	getListJobsQueryKey,
	useCancelJob,
	useRerunJob,
} from "@/api/gen/endpoints";
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
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const TOAST_MS = 8000;

export type CancellableJob = { id: string; state: string };

export function CancelJobDialog({
	job,
	onClose,
	onCloseAutoFocus,
}: {
	job?: CancellableJob;
	onClose: () => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const queryClient = useQueryClient();
	const cancel = useCancelJob();

	function close() {
		cancel.reset();
		onClose();
	}

	function confirm() {
		if (!job) return;
		cancel.mutate(
			{ id: job.id },
			{
				onSuccess: () => {
					toast.success(copy.jobs.cancel.toast);
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
					void queryClient.invalidateQueries({
						queryKey: getGetJobQueryKey(job.id),
					});
					close();
				},
			},
		);
	}

	return (
		<AlertDialog
			open={job !== undefined}
			onOpenChange={(open) => {
				if (!open && !cancel.isPending) close();
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle>{copy.jobs.cancel.heading}</AlertDialogTitle>
					<AlertDialogDescription>
						{job?.state === "queued"
							? copy.jobs.cancel.bodyQueued
							: copy.jobs.cancel.body}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{cancel.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.errors.submit(copy.jobs.cancelFailed, cancel.error.message)}
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={cancel.isPending}>
						{copy.jobs.cancel.keep}
					</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={cancel.isPending}
						onClick={(event) => {
							event.preventDefault();
							confirm();
						}}
					>
						{cancel.isPending && <Spinner />}
						{copy.jobs.cancel.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

export function useRerunAction({ open = false }: { open?: boolean } = {}) {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const rerun = useRerunJob();
	return {
		pending: rerun.isPending,
		run(job: { id: string }) {
			rerun.mutate(
				{ id: job.id },
				{
					onSuccess: (run) => {
						void queryClient.invalidateQueries({
							queryKey: getListJobsQueryKey(),
						});
						const show = () =>
							navigate({ to: "/jobs/$id", params: { id: run.id } });
						if (open) return void show();
						toast.success(
							run.created
								? copy.jobs.run.queuedOne(run.hostname)
								: copy.jobs.run.allAlreadyQueued(run.hostname),
							{
								duration: TOAST_MS,
								action: {
									label: copy.jobs.viewJob,
									onClick: () => void show(),
								},
							},
						);
					},
					onError: (error) =>
						toast.error(
							copy.errors.submit(copy.jobs.rerunFailed, error.message),
						),
				},
			);
		},
	};
}
