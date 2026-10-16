import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { TriangleAlert } from "lucide-react";
import { toast } from "sonner";
import {
	getListJobsQueryKey,
	getListPoliciesQueryKey,
	useGetPolicy,
	useRunPolicy,
} from "@/api/gen/endpoints";
import type { RunResult } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { warningText } from "@/components/policies/matching-hosts";
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
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const VISIBLE = 10;
const TOAST_MS = 8000;

function runMessage(result: RunResult) {
	const total = result.jobs.length;
	const merged = result.jobs.filter((job) => !job.created).length;
	if (total === 1) {
		const [job] = result.jobs;
		return merged === 1
			? copy.jobs.run.allAlreadyQueued(job.hostname)
			: copy.jobs.run.queuedOne(job.hostname);
	}
	const base = copy.jobs.run.queuedMany(total);
	return merged > 0 ? `${base} ${copy.jobs.run.alreadyQueued(merged)}` : base;
}

export function RunNowDialog({
	name,
	onClose,
	onCloseAutoFocus,
}: {
	name?: string;
	onClose: () => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const detail = useGetPolicy(name ?? "", {
		query: { enabled: name !== undefined },
	});
	const run = useRunPolicy();

	function close() {
		run.reset();
		onClose();
	}

	function confirm() {
		if (!name) return;
		run.mutate(
			{ name },
			{
				onSuccess: (result) => {
					const single = result.jobs.length === 1 ? result.jobs[0] : undefined;
					toast.success(runMessage(result), {
						duration: TOAST_MS,
						action: {
							label: single ? copy.jobs.viewJob : copy.jobs.viewJobs,
							onClick: () =>
								single
									? void navigate({
											to: "/jobs/$id",
											params: { id: single.id },
										})
									: void navigate({
											to: "/jobs",
											search: { policy: name } as never,
										}),
						},
					});
					void queryClient.invalidateQueries({
						queryKey: getListPoliciesQueryKey(),
					});
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
					close();
				},
			},
		);
	}

	const hosts = detail.data?.matching.hosts ?? [];
	const skipped = detail.data?.matching.other_repository.length ?? 0;
	const warnings = detail.data?.warnings ?? [];
	const text = copy.policies.run;

	return (
		<Dialog
			open={name !== undefined}
			onOpenChange={(open) => {
				if (!open && !run.isPending) close();
			}}
		>
			<DialogContent
				aria-describedby={undefined}
				onCloseAutoFocus={onCloseAutoFocus}
			>
				<DialogHeader>
					<DialogTitle>{text.title(name ?? "")}</DialogTitle>
					<DialogDescription>{text.body}</DialogDescription>
				</DialogHeader>

				{detail.isPending && name !== undefined && (
					<div className="flex flex-col gap-2" aria-hidden>
						<Skeleton className="h-4 w-48" />
						<Skeleton className="h-4 w-40" />
					</div>
				)}
				{detail.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{copy.policies.matching.loadFailed}{" "}
							<Button
								variant="link"
								className="h-auto p-0"
								onClick={() => void detail.refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				)}
				{detail.data && hosts.length === 0 && (
					<p className="text-sm text-muted-foreground">{text.nothing}</p>
				)}
				{hosts.length > 0 && (
					<ul className="flex flex-col gap-1 font-semibold">
						{hosts.slice(0, VISIBLE).map((host) => (
							<li key={host.id}>{host.hostname}</li>
						))}
						{hosts.length > VISIBLE && (
							<li className="font-normal text-muted-foreground">
								{text.more(hosts.length - VISIBLE)}
							</li>
						)}
					</ul>
				)}
				{warnings.length > 0 && (
					<ul className="flex flex-col gap-1 text-sm text-warning">
						{warnings.map((warning, i) => (
							<li key={i} className="flex items-start gap-1">
								<TriangleAlert aria-hidden className="mt-1 size-4 shrink-0" />
								<span>
									{warning.hostname}: {warningText(warning)}
								</span>
							</li>
						))}
					</ul>
				)}
				{skipped > 0 && (
					<p className="text-sm text-muted-foreground">
						{text.skipped(skipped)}
					</p>
				)}
				{run.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							<TextWithPaths
								text={copy.errors.submit(text.failed, run.error.message)}
							/>
						</AlertDescription>
					</Alert>
				)}

				<DialogFooter>
					<Button
						type="button"
						variant="outline"
						disabled={run.isPending}
						onClick={close}
					>
						{text.dismiss}
					</Button>
					{detail.data && (
						<Button
							type="button"
							autoFocus
							disabled={run.isPending || hosts.length === 0}
							onClick={confirm}
						>
							{run.isPending && <Spinner />}
							{text.confirm(hosts.length)}
						</Button>
					)}
				</DialogFooter>
			</DialogContent>
		</Dialog>
	);
}
