import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { ChevronDown } from "lucide-react";
import { toast } from "sonner";
import {
	getListHostPoliciesQueryKey,
	getListJobsQueryKey,
	useListHostPolicies,
	useRunHost,
} from "@/api/gen/endpoints";
import type { HostDetail, RunResult } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const TOAST_MS = 8_000;

export function RunBackupButton({ host }: { host: HostDetail }) {
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const policies = useListHostPolicies(host.id, {
		query: { enabled: host.status !== "revoked" },
	});
	const run = useRunHost();
	const names = policies.data?.map((p) => p.name) ?? [];
	if (host.status === "revoked" || names.length === 0) return null;

	function announce({ jobs }: RunResult) {
		const merged = jobs.filter((job) => !job.created).length;
		const text =
			merged === jobs.length
				? copy.jobs.run.allAlreadyQueued(host.hostname)
				: [
						jobs.length === 1
							? copy.jobs.run.queuedOne(host.hostname)
							: copy.jobs.run.queuedMany(jobs.length),
						merged > 0 ? copy.jobs.run.alreadyQueued(merged) : null,
					]
						.filter(Boolean)
						.join(" ");
		const [first] = jobs;
		toast.success(text, {
			duration: TOAST_MS,
			action: {
				label: jobs.length === 1 ? copy.jobs.viewJob : copy.jobs.viewJobs,
				onClick: () =>
					void (jobs.length === 1
						? navigate({ to: "/jobs/$id", params: { id: first.id } })
						: navigate({ to: "/jobs", search: { host: host.hostname } })),
			},
		});
	}

	function start(selected: string[] | null) {
		run.mutate(
			{ id: host.id, data: { policies: selected } },
			{
				onSuccess: (result) => {
					announce(result);
					void queryClient.invalidateQueries({
						queryKey: getListHostPoliciesQueryKey(host.id),
					});
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
				},
				onError: (error) =>
					toast.error(
						copy.errors.submit(copy.hostBackup.startFailed, error.message),
					),
			},
		);
	}

	if (names.length === 1) {
		return (
			<Button disabled={run.isPending} onClick={() => start(names)}>
				{run.isPending && <Spinner />}
				{copy.hostBackup.runBackup}
			</Button>
		);
	}

	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button disabled={run.isPending}>
					{run.isPending && <Spinner />}
					{copy.hostBackup.runBackup}
					<ChevronDown aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				{names.map((name) => (
					<DropdownMenuItem key={name} onSelect={() => start([name])}>
						{name}
					</DropdownMenuItem>
				))}
				<DropdownMenuSeparator />
				<DropdownMenuItem onSelect={() => start(null)}>
					{copy.hostBackup.allPolicies(names.length)}
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}
