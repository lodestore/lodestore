import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { getListTokensQueryKey, useRevokeToken } from "@/api/gen/endpoints";
import type { ApiTokenItem } from "@/api/gen/model";
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

const text = copy.tokens.revoke;
const TOAST_MS = 4000;

export function RevokeTokenDialog({
	token,
	onClose,
	onCloseAutoFocus,
}: {
	token?: ApiTokenItem;
	onClose: (revoked: boolean) => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const queryClient = useQueryClient();
	const revoke = useRevokeToken();

	function close(revoked: boolean) {
		revoke.reset();
		onClose(revoked);
	}

	function confirm() {
		if (!token) return;
		revoke.mutate(
			{ id: token.id },
			{
				onSuccess: () => {
					toast.success(text.done(token.name), { duration: TOAST_MS });
					void queryClient.invalidateQueries({
						queryKey: getListTokensQueryKey(),
					});
					close(true);
				},
			},
		);
	}

	return (
		<AlertDialog
			open={token !== undefined}
			onOpenChange={(open) => {
				if (!open && !revoke.isPending) close(false);
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle className="wrap-anywhere">
						{text.title(token?.name ?? "")}
					</AlertDialogTitle>
					<AlertDialogDescription>{text.body}</AlertDialogDescription>
				</AlertDialogHeader>
				{revoke.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.errors.submit(text.action, revoke.error.message)}
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={revoke.isPending}>
						{text.keep}
					</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						disabled={revoke.isPending}
						onClick={(event) => {
							event.preventDefault();
							confirm();
						}}
					>
						{revoke.isPending && <Spinner />}
						{text.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
