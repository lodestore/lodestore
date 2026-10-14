import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import {
	getGetHostQueryKey,
	getListHostsQueryKey,
	useRevokeHost,
} from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
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

export function RevokeHostDialog({
	host,
	onClose,
	onCloseAutoFocus,
}: {
	host?: Host;
	onClose: () => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const queryClient = useQueryClient();
	const revoke = useRevokeHost();

	function close() {
		revoke.reset();
		onClose();
	}

	function confirm() {
		if (!host) return;
		revoke.mutate(
			{ id: host.id },
			{
				onSuccess: () => {
					toast.success(copy.hosts.hostRevoked(host.hostname));
					void queryClient.invalidateQueries({
						queryKey: getListHostsQueryKey(),
					});
					void queryClient.invalidateQueries({
						queryKey: getGetHostQueryKey(host.id),
					});
					close();
				},
			},
		);
	}

	return (
		<AlertDialog
			open={host !== undefined}
			onOpenChange={(open) => {
				if (!open && !revoke.isPending) close();
			}}
		>
			<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
				<AlertDialogHeader>
					<AlertDialogTitle>
						{copy.hosts.revokeTitle(host?.hostname ?? "")}
					</AlertDialogTitle>
					<AlertDialogDescription>
						{copy.hosts.revokeBody}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{host && (
					<Link
						to="/settings/repositories/$id"
						params={{ id: host.repository_id }}
						onClick={close}
						className="text-sm font-semibold underline underline-offset-4"
					>
						{copy.hosts.replaceKeyLink}
					</Link>
				)}
				{revoke.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.errors.submit("revoke the host", revoke.error.message)}
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={revoke.isPending}>
						{copy.wizard.keepHost}
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
						{copy.hosts.revokeHost}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
