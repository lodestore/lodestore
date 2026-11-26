import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { toast } from "sonner";
import {
	getGetHostQueryKey,
	getGetRepositoryQueryOptions,
	getListHostsQueryKey,
	getStorageAccessQueryOptions,
	useRevokeHost,
} from "@/api/gen/endpoints";
import type { Host, StorageAccess } from "@/api/gen/model";
import { LimitAgentRightsSheet } from "@/components/repos/limit-agent-rights-sheet";
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
import { formatDateTime } from "@/lib/format";

type Followup = {
	hostname: string;
	repositoryId: string;
	text: string;
	replace: boolean;
};

const SHARED = new Set(["server_key_shared", "new_repository"]);

function followupOf(
	access: StorageAccess,
	hostname: string,
	repositoryName: () => Promise<string>,
	repositoryId: string,
): Promise<Followup | undefined> | undefined {
	if (
		access.after_revoke === "expires" &&
		access.access_ends_by != null &&
		access.access_ends_by > Date.now()
	) {
		return Promise.resolve({
			hostname,
			repositoryId,
			text: copy.revoke.endsBy(formatDateTime(access.access_ends_by)),
			replace: false,
		});
	}
	if (access.status !== "revoked_hosts_hold_access") return undefined;
	if (SHARED.has(access.after_revoke)) {
		return repositoryName().then((name) => ({
			hostname,
			repositoryId,
			text: copy.revoke.sharedHolds(hostname, name),
			replace: false,
		}));
	}
	if (!access.rotate_via) return undefined;
	return repositoryName().then((name) => ({
		hostname,
		repositoryId,
		text: copy.revoke.stillWrites(hostname, name),
		replace: true,
	}));
}

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
	const [resolving, setResolving] = useState(false);
	const [followup, setFollowup] = useState<Followup>();
	const [sheetFor, setSheetFor] = useState<string>();
	const [sheetOpen, setSheetOpen] = useState(false);

	function close() {
		revoke.reset();
		setFollowup(undefined);
		onClose();
	}

	async function resolveFollowup(revoked: Host) {
		try {
			const id = revoked.repository_id;
			const access = await queryClient.fetchQuery(
				getStorageAccessQueryOptions(id),
			);
			const next = await followupOf(
				access,
				revoked.hostname,
				async () =>
					(await queryClient.fetchQuery(getGetRepositoryQueryOptions(id))).name,
				id,
			);
			if (next) setFollowup(next);
			else close();
		} catch {
			close();
		} finally {
			setResolving(false);
		}
	}

	function confirm() {
		if (!host) return;
		const revoked = host;
		revoke.mutate(
			{ id: revoked.id },
			{
				onSuccess: () => {
					toast.success(copy.hosts.hostRevoked(revoked.hostname));
					void queryClient.invalidateQueries({
						queryKey: getListHostsQueryKey(),
					});
					void queryClient.invalidateQueries({
						queryKey: getGetHostQueryKey(revoked.id),
					});
					setResolving(true);
					void resolveFollowup(revoked);
				},
			},
		);
	}

	const busy = revoke.isPending || resolving;

	return (
		<>
			<AlertDialog
				open={host !== undefined || followup !== undefined}
				onOpenChange={(open) => {
					if (!open && !busy) close();
				}}
			>
				<AlertDialogContent onCloseAutoFocus={onCloseAutoFocus}>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{followup
								? copy.hosts.revokedTitle(followup.hostname)
								: copy.hosts.revokeTitle(host?.hostname ?? "")}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{followup ? followup.text : copy.hosts.revokeBody}
						</AlertDialogDescription>
					</AlertDialogHeader>
					{host && !followup && (
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
						{followup ? (
							followup.replace ? (
								<>
									<AlertDialogCancel onClick={close}>
										{copy.revoke.later}
									</AlertDialogCancel>
									<AlertDialogAction
										onClick={() => {
											const repositoryId = followup.repositoryId;
											close();
											setSheetFor(repositoryId);
											setSheetOpen(true);
										}}
									>
										{copy.revoke.replaceKey}
									</AlertDialogAction>
								</>
							) : (
								<AlertDialogAction onClick={close}>
									{copy.limit.close}
								</AlertDialogAction>
							)
						) : (
							<>
								<AlertDialogCancel disabled={busy}>
									{copy.wizard.keepHost}
								</AlertDialogCancel>
								<AlertDialogAction
									variant="destructive"
									disabled={busy}
									onClick={(event) => {
										event.preventDefault();
										confirm();
									}}
								>
									{busy && <Spinner />}
									{copy.hosts.revokeHost}
								</AlertDialogAction>
							</>
						)}
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
			{sheetFor && (
				<LimitAgentRightsSheet
					repositoryId={sheetFor}
					open={sheetOpen}
					onOpenChange={setSheetOpen}
				/>
			)}
		</>
	);
}
