import { useQueryClient } from "@tanstack/react-query";
import { ChevronDown, CircleAlert, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { getGetStatusQueryKey, useExclude } from "@/api/gen/endpoints";
import type { GcQuarantineDto } from "@/api/gen/model";
import { PathText, TextWithPaths } from "@/components/path-text";
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
	AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const text = copy.gc.quarantine;
export const QUARANTINE_SHOWN = 10;

export function ExcludeSnapshotDialog({
	repositoryId,
	item,
}: {
	repositoryId: string;
	item: GcQuarantineDto;
}) {
	const client = useQueryClient();
	const [open, setOpen] = useState(false);
	const refresh = () =>
		client.invalidateQueries({ queryKey: getGetStatusQueryKey(repositoryId) });
	const exclude = useExclude({
		mutation: {
			onSuccess: () => {
				setOpen(false);
				toast.success(text.done);
				void refresh();
			},
			onError: (error) => {
				if (error.status === 404) void refresh();
			},
		},
	});

	return (
		<AlertDialog
			open={open}
			onOpenChange={(next) => {
				if (exclude.isPending) return;
				setOpen(next);
				if (!next) exclude.reset();
			}}
		>
			<AlertDialogTrigger asChild>
				<Button type="button" variant="outline" size="sm" className="w-fit">
					{text.exclude}
				</Button>
			</AlertDialogTrigger>
			<AlertDialogContent className="data-[size=default]:sm:max-w-[560px]">
				<AlertDialogHeader>
					<AlertDialogTitle>{text.dialogTitle}</AlertDialogTitle>
					<AlertDialogDescription className="wrap-anywhere">
						{text.dialogBody}
					</AlertDialogDescription>
				</AlertDialogHeader>
				<p className="font-mono text-sm">
					<PathText value={item.key} />
				</p>
				{exclude.isError && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertDescription>
							<TextWithPaths
								text={copy.errors.submit(text.failed, exclude.error.message)}
							/>
						</AlertDescription>
					</Alert>
				)}
				<AlertDialogFooter>
					<AlertDialogCancel disabled={exclude.isPending}>
						{text.cancel}
					</AlertDialogCancel>
					<AlertDialogAction
						variant="destructive"
						aria-disabled={exclude.isPending || undefined}
						onClick={(event) => {
							event.preventDefault();
							if (exclude.isPending) return;
							exclude.mutate({
								id: repositoryId,
								snapshotId: item.snapshot_id,
							});
						}}
					>
						{exclude.isPending && <Spinner />}
						{text.confirm}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}

export function QuarantineAlert({
	repositoryId,
	items,
}: {
	repositoryId: string;
	items: GcQuarantineDto[];
}) {
	const open = items.filter((item) => item.confirmed_at == null);
	const shown = open.slice(0, QUARANTINE_SHOWN);
	const more = open.length - shown.length;
	return (
		<Alert className="text-warning">
			<TriangleAlert aria-hidden />
			<AlertDescription className="flex flex-col gap-2 text-foreground [&_p:not(:last-child)]:mb-0">
				<p className="font-semibold">{text.title(open.length)}</p>
				<p>{text.description}</p>
				<Collapsible defaultOpen>
					<CollapsibleTrigger className="group flex items-center gap-1 text-sm font-semibold outline-none focus-visible:ring-2 focus-visible:ring-ring">
						{text.list}
						<ChevronDown
							aria-hidden
							className="size-4 transition-transform group-data-[state=open]:rotate-180"
						/>
					</CollapsibleTrigger>
					<CollapsibleContent>
						<ul className="mt-2 flex flex-col gap-4">
							{shown.map((item) => (
								<li
									key={item.snapshot_id}
									className="flex flex-col gap-1 text-sm"
								>
									<span className="font-mono">
										<PathText value={item.key} />
									</span>
									<span className="text-muted-foreground">{item.reason}</span>
									{item.confirmable ? (
										<ExcludeSnapshotDialog
											repositoryId={repositoryId}
											item={item}
										/>
									) : (
										<span className="text-muted-foreground">{text.bound}</span>
									)}
								</li>
							))}
							{more > 0 && (
								<li className="text-sm text-muted-foreground">
									{text.more(more)}
								</li>
							)}
						</ul>
					</CollapsibleContent>
				</Collapsible>
			</AlertDescription>
		</Alert>
	);
}
