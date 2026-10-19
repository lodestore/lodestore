import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert, TriangleAlert } from "lucide-react";
import { useRef, useState } from "react";
import { toast } from "sonner";
import {
	getGetStatusQueryKey,
	getListJobsQueryKey,
	useUnlock,
} from "@/api/gen/endpoints";
import type { WriterMarkerDto } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { STATUS_ID } from "@/components/repos/reclaim/use-gc-status";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
	AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const text = copy.gc.markers;
const ROWS_SHOWN = 5;
const HOSTS_NAMED = 10;

const hostOf = (marker: WriterMarkerDto) => marker.host ?? text.unknownHost;

export function UnlockMarkersDialog({
	repositoryId,
	markers,
}: {
	repositoryId: string;
	markers: WriterMarkerDto[];
}) {
	const client = useQueryClient();
	const [open, setOpen] = useState(false);
	const unlocked = useRef(false);
	const unlock = useUnlock({
		mutation: {
			onSuccess: (result) => {
				unlocked.current = true;
				setOpen(false);
				toast.success(text.done(result.unlocked));
				void client.invalidateQueries({
					queryKey: getGetStatusQueryKey(repositoryId),
				});
				void client.invalidateQueries({ queryKey: getListJobsQueryKey() });
			},
			onError: (error) => {
				void client.invalidateQueries({
					queryKey: getGetStatusQueryKey(repositoryId),
				});
				if (error.status !== 409) return;
				setOpen(false);
				toast.error(copy.errors.submit(text.failed, error.message));
			},
		},
	});
	const hosts = [...new Set(markers.map(hostOf))];
	const named = hosts.slice(0, HOSTS_NAMED).join(", ");
	const rest = hosts.length - HOSTS_NAMED;

	return (
		<AlertDialog
			open={open}
			onOpenChange={(next) => {
				if (unlock.isPending) return;
				setOpen(next);
				if (next) unlocked.current = false;
				else unlock.reset();
			}}
		>
			<AlertDialogTrigger asChild>
				<Button type="button" variant="outline" size="sm" className="w-fit">
					{text.unlock(markers.length)}
				</Button>
			</AlertDialogTrigger>
			<AlertDialogContent
				className="data-[size=default]:sm:max-w-[560px]"
				onCloseAutoFocus={(event) => {
					if (!unlocked.current) return;
					event.preventDefault();
					document.getElementById(STATUS_ID)?.focus();
				}}
			>
				<AlertDialogHeader>
					<AlertDialogTitle>
						{text.dialogTitle(markers.length)}
					</AlertDialogTitle>
					<AlertDialogDescription className="wrap-anywhere">
						{text.dialogBody(rest > 0 ? `${named} ${text.more(rest)}` : named)}
					</AlertDialogDescription>
				</AlertDialogHeader>
				{unlock.isError && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertTitle>{text.failedTitle}</AlertTitle>
						<AlertDescription>
							<TextWithPaths
								text={copy.errors.submit(text.failed, unlock.error.message)}
							/>
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={unlock.isPending}>
						{text.keep}
					</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						aria-disabled={unlock.isPending || undefined}
						onClick={(event) => {
							event.preventDefault();
							if (unlock.isPending) return;
							unlock.mutate({ id: repositoryId });
						}}
					>
						{unlock.isPending && <Spinner />}
						{text.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

export function WriterMarkersAlert({
	repositoryId,
	markers,
}: {
	repositoryId: string;
	markers: WriterMarkerDto[];
}) {
	const shown = markers.slice(0, ROWS_SHOWN);
	const more = markers.length - shown.length;
	return (
		<Alert className="text-warning">
			<TriangleAlert aria-hidden />
			<AlertTitle>{text.title}</AlertTitle>
			<AlertDescription className="flex flex-col gap-2 text-foreground [&_p:not(:last-child)]:mb-0">
				<p>{text.body}</p>
				<ul className="flex flex-col gap-1">
					{shown.map((marker) => (
						<li key={marker.key} className="wrap-anywhere">
							<span className="font-semibold">{hostOf(marker)}</span>
							<span className="text-muted-foreground">
								{text.source}
								{marker.started_at != null && (
									<>
										{text.started}
										<RelativeTime value={new Date(marker.started_at)} />
									</>
								)}
							</span>
						</li>
					))}
					{more > 0 && (
						<li className="text-muted-foreground">{text.more(more)}</li>
					)}
				</ul>
				<UnlockMarkersDialog repositoryId={repositoryId} markers={markers} />
			</AlertDescription>
		</Alert>
	);
}
