import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert, Fingerprint, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ApiError } from "@/api/fetcher";
import {
	getGetRepositoryQueryKey,
	useCheckHostKey,
	useReplaceHostKey,
} from "@/api/gen/endpoints";
import type { HostKeyOffer } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

export function HostKeyRecheck({ repositoryId }: { repositoryId: string }) {
	const queryClient = useQueryClient();
	const [offer, setOffer] = useState<HostKeyOffer>();
	const [error, setError] = useState<string>();
	const check = useCheckHostKey();
	const trust = useReplaceHostKey();

	function close() {
		setOffer(undefined);
		setError(undefined);
		trust.reset();
	}

	function onCheck() {
		check.mutate(
			{ id: repositoryId },
			{
				onSuccess: (result) =>
					result.matches ? toast.success(copy.hostKey.same) : setOffer(result),
				onError: (e) =>
					toast.error(
						copy.errors.submit(copy.hostKey.recheckFailed, e.message),
					),
			},
		);
	}

	async function onTrust() {
		if (!offer) return;
		setError(undefined);
		try {
			await trust.mutateAsync({
				id: repositoryId,
				data: { host_key: offer.key },
			});
			toast.success(copy.hostKey.trusted);
			close();
			await queryClient.invalidateQueries({
				queryKey: getGetRepositoryQueryKey(repositoryId),
			});
		} catch (e) {
			if (e instanceof ApiError && e.code === "step_up_required") return;
			setError(
				copy.errors.submit(
					copy.hostKey.recheckFailed,
					e instanceof Error ? e.message : undefined,
				),
			);
		}
	}

	return (
		<>
			<Button
				type="button"
				variant="outline"
				disabled={check.isPending}
				onClick={onCheck}
			>
				{check.isPending ? <Spinner /> : <Fingerprint aria-hidden />}
				{copy.hostKey.recheck}
			</Button>
			<Dialog
				open={offer !== undefined}
				onOpenChange={(open) => !open && !trust.isPending && close()}
			>
				<DialogContent className="sm:max-w-[480px]">
					<DialogHeader>
						<DialogTitle>{copy.hostKey.changedTitle}</DialogTitle>
						<DialogDescription className="sr-only">
							{copy.hostKey.changedBody}
						</DialogDescription>
					</DialogHeader>
					<dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-sm">
						<dt className="text-muted-foreground">{copy.hostKey.pinned}</dt>
						<dd className="font-mono wrap-anywhere">
							{offer?.pinned_fingerprint}
						</dd>
						<dt className="text-muted-foreground">{copy.hostKey.presented}</dt>
						<dd className="font-mono wrap-anywhere">{offer?.fingerprint}</dd>
					</dl>
					<Alert className="text-warning">
						<TriangleAlert aria-hidden />
						<AlertDescription>{copy.hostKey.changedBody}</AlertDescription>
					</Alert>
					{error && (
						<Alert variant="destructive" role="alert">
							<CircleAlert aria-hidden />
							<AlertDescription>{error}</AlertDescription>
						</Alert>
					)}
					<DialogFooter>
						<Button
							type="button"
							variant="outline"
							disabled={trust.isPending}
							onClick={close}
						>
							{copy.hostKey.keep}
						</Button>
						<Button
							type="button"
							disabled={trust.isPending}
							onClick={() => void onTrust()}
						>
							{trust.isPending && <Spinner />}
							{copy.hostKey.trust}
						</Button>
					</DialogFooter>
				</DialogContent>
			</Dialog>
		</>
	);
}
