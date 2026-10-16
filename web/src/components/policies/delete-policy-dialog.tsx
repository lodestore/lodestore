import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { getListPoliciesQueryKey, useDeletePolicy } from "@/api/gen/endpoints";
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

export function DeletePolicyDialog({
	name,
	onClose,
	onDeleted,
	onCloseAutoFocus,
}: {
	name?: string;
	onClose: () => void;
	onDeleted?: () => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const queryClient = useQueryClient();
	const remove = useDeletePolicy();

	function close() {
		remove.reset();
		onClose();
	}

	function confirm() {
		if (!name) return;
		remove.mutate(
			{ name },
			{
				onSuccess: () => {
					toast.success(copy.policies.remove.toast(name));
					void queryClient.invalidateQueries({
						queryKey: getListPoliciesQueryKey(),
					});
					onDeleted?.();
					close();
				},
			},
		);
	}

	return (
		<AlertDialog
			open={name !== undefined}
			onOpenChange={(open) => {
				if (!open && !remove.isPending) close();
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle>
						{copy.policies.remove.heading(name ?? "")}
					</AlertDialogTitle>
					<AlertDialogDescription>
						{copy.policies.remove.body}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{remove.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>{remove.error.message}</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={remove.isPending}>
						{copy.policies.remove.keep}
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
						{copy.policies.remove.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
