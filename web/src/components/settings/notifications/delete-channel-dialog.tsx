import { useQueryClient } from "@tanstack/react-query";
import { useRef } from "react";
import { toast } from "sonner";
import { getListChannelsQueryKey, useDeleteChannel } from "@/api/gen/endpoints";
import type { ChannelDto } from "@/api/gen/model";
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

const text = copy.notifications.delete;
const TOAST_MS = 4000;

export function DeleteChannelDialog({
	channel,
	onClose,
	onCloseAutoFocus,
}: {
	channel?: ChannelDto;
	onClose: (deleted: boolean) => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const queryClient = useQueryClient();
	const remove = useDeleteChannel();
	const lastName = useRef("");
	if (channel) lastName.current = channel.name;

	function close(deleted: boolean) {
		remove.reset();
		onClose(deleted);
	}

	function confirm() {
		if (!channel) return;
		remove.mutate(
			{ id: channel.id },
			{
				onSuccess: () => {
					toast.success(text.done, { duration: TOAST_MS });
					close(true);
				},
				onSettled: () =>
					void queryClient.invalidateQueries({
						queryKey: getListChannelsQueryKey(),
					}),
			},
		);
	}

	return (
		<AlertDialog
			open={channel !== undefined}
			onOpenChange={(open) => {
				if (!open && !remove.isPending) close(false);
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle className="wrap-anywhere">
						{text.title(lastName.current)}
					</AlertDialogTitle>
					<AlertDialogDescription>{text.body}</AlertDialogDescription>
				</AlertDialogHeader>
				{remove.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.errors.submit(text.action, remove.error.message)}
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={remove.isPending}>
						{text.keep}
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
						{text.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
