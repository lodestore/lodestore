import { Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import type { PolicySummary } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { ManagedBadge } from "@/components/policies/managed";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const SHOW_SELECTOR = "hidden @min-[1280px]:table-cell";
const SHOW_SCHEDULE = "hidden @min-[1024px]:table-cell";
const SHOW_LAST_RUN = "hidden @min-[1024px]:table-cell";
const SHOWN_CHIPS = 2;

function SelectorCell({ policy }: { policy: PolicySummary }) {
	const labels = policy.match_labels;
	if (policy.host_list) {
		return <span>{copy.policies.pickedHosts}</span>;
	}
	if (labels.length === 0) {
		return (
			<span className="text-muted-foreground">{copy.policies.allHosts}</span>
		);
	}
	const rest = labels.slice(SHOWN_CHIPS);
	return (
		<div className="flex flex-wrap items-center gap-1">
			{labels.slice(0, SHOWN_CHIPS).map((label) => (
				<Badge
					key={label.key}
					variant="outline"
					className="h-6 max-w-32 min-w-0 font-mono"
				>
					<span className="truncate">{`${label.key}=${label.value}`}</span>
				</Badge>
			))}
			{rest.length > 0 && (
				<Tooltip>
					<TooltipTrigger asChild>
						<Badge variant="secondary">
							{copy.policies.moreLabels(rest.length)}
						</Badge>
					</TooltipTrigger>
					<TooltipContent>
						<ul className="font-mono">
							{rest.map((label) => (
								<li key={label.key}>{`${label.key}=${label.value}`}</li>
							))}
						</ul>
					</TooltipContent>
				</Tooltip>
			)}
		</div>
	);
}

function ScheduleCell({ policy }: { policy: PolicySummary }) {
	const info = policy.schedule;
	if (!info) {
		return (
			<span className="text-muted-foreground">{copy.policies.manualOnly}</span>
		);
	}
	const next = info.next_runs[0];
	return (
		<div className="flex max-w-56 flex-col">
			<Tooltip>
				<TooltipTrigger asChild>
					<span tabIndex={0} className="truncate rounded-sm">
						{info.summary}
					</span>
				</TooltipTrigger>
				<TooltipContent>
					{copy.policies.scheduleHint(info.summary, info.timezone)}
				</TooltipContent>
			</Tooltip>
			{next !== undefined && (
				<span className="text-sm text-muted-foreground">
					{copy.policies.next}{" "}
					<RelativeTime value={new Date(next)} numeric="always" />
				</span>
			)}
		</div>
	);
}

function LastRun({ policy }: { policy: PolicySummary }) {
	const job = policy.last_job;
	if (!job) {
		return (
			<span className="text-muted-foreground">{copy.policies.neverRun}</span>
		);
	}
	return (
		<div className="flex flex-col items-start gap-1">
			<StatusBadge status={job.state} />
			<span className="text-sm text-muted-foreground">
				<RelativeTime value={new Date(job.finished_at ?? job.queued_at)} />
			</span>
		</div>
	);
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden className="h-16">
			<TableCell className="px-4">
				<Skeleton className="h-4 w-40" />
				<Skeleton className="mt-2 h-3 w-16" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_SELECTOR)}>
				<Skeleton className="h-5 w-32" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_SCHEDULE)}>
				<Skeleton className="h-4 w-32" />
				<Skeleton className="mt-2 h-3 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-4 w-12" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_LAST_RUN)}>
				<Skeleton className="h-5 w-24" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="size-10" />
			</TableCell>
		</TableRow>
	));
}

function PoliciesEmpty({ hasRepositories }: { hasRepositories: boolean }) {
	return (
		<Empty className="py-24">
			<EmptyHeader>
				<Illustration
					name="preview-policies"
					width={320}
					height={216}
					className="h-auto w-80 max-w-full"
				/>
				<EmptyTitle>{copy.policies.emptyHeading}</EmptyTitle>
				<EmptyDescription>
					{hasRepositories ? copy.policies.emptyBody : copy.policies.noRepoBody}
				</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					{hasRepositories ? (
						<Link to="/policies/new">{copy.policies.create}</Link>
					) : (
						<Link to="/settings/repositories/new">
							{copy.policies.addRepository}
						</Link>
					)}
				</Button>
			</EmptyContent>
		</Empty>
	);
}

export function PoliciesTable({
	policies,
	isPending,
	isError,
	onRetry,
	hasRepositories,
	onRun,
	onDelete,
}: {
	policies?: PolicySummary[];
	isPending: boolean;
	isError: boolean;
	onRetry: () => void;
	hasRepositories: boolean;
	onRun: (name: string) => void;
	onDelete: (name: string) => void;
}) {
	const navigate = useNavigate();
	const open = (name: string) =>
		navigate({ to: "/policies/$name", params: { name } });

	const error = isError && (
		<Alert variant="destructive" role="alert">
			<AlertDescription className="flex flex-wrap items-center justify-between gap-2">
				{copy.policies.loadFailed}
				<Button type="button" variant="outline" size="sm" onClick={onRetry}>
					{copy.errors.retry}
				</Button>
			</AlertDescription>
		</Alert>
	);
	if (isError && policies === undefined) return error;
	if (policies?.length === 0) {
		return (
			<>
				{error}
				<PoliciesEmpty hasRepositories={hasRepositories} />
			</>
		);
	}

	return (
		<div className="flex flex-col gap-4">
			{error}
			<div className="@container overflow-hidden rounded-lg border bg-card">
				<Table>
					<TableHeader className="bg-muted">
						<TableRow>
							<TableHead className="px-4">
								{copy.policies.columns.name}
							</TableHead>
							<TableHead className={cn("px-4", SHOW_SELECTOR)}>
								{copy.policies.columns.selector}
							</TableHead>
							<TableHead className={cn("px-4", SHOW_SCHEDULE)}>
								{copy.policies.columns.schedule}
							</TableHead>
							<TableHead className="px-4">
								{copy.policies.columns.hosts}
							</TableHead>
							<TableHead className={cn("px-4", SHOW_LAST_RUN)}>
								{copy.policies.columns.lastRun}
							</TableHead>
							<TableHead className="w-16 px-4">
								<span className="sr-only">{copy.policies.columns.actions}</span>
							</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{policies === undefined
							? isPending && <SkeletonRows />
							: policies.map((policy) => (
									<TableRow
										key={policy.name}
										className="h-16 cursor-pointer"
										onClick={(e) => {
											const target = e.target as HTMLElement;
											if (!target.closest("a,button,[role=menuitem]")) {
												void open(policy.name);
											}
										}}
									>
										<TableCell className="px-4">
											<div className="flex max-w-64 items-center gap-2">
												<Link
													to="/policies/$name"
													params={{ name: policy.name }}
													aria-label={copy.policies.openRow(policy.name)}
													className="min-w-0 truncate rounded-sm text-base font-semibold"
												>
													{policy.name}
												</Link>
												{policy.managed_by && (
													<ManagedBadge manager={policy.managed_by} />
												)}
											</div>
											<div className="max-w-64 truncate text-sm text-muted-foreground">
												{policy.repository_name}
											</div>
										</TableCell>
										<TableCell className={cn("px-4", SHOW_SELECTOR)}>
											<SelectorCell policy={policy} />
										</TableCell>
										<TableCell className={cn("px-4", SHOW_SCHEDULE)}>
											<ScheduleCell policy={policy} />
										</TableCell>
										<TableCell className="px-4">
											<div>{policy.hosts}</div>
											{policy.other_repository_hosts > 0 && (
												<div className="text-sm text-muted-foreground">
													{copy.policies.otherRepos(
														policy.other_repository_hosts,
													)}
												</div>
											)}
										</TableCell>
										<TableCell className={cn("px-4", SHOW_LAST_RUN)}>
											<LastRun policy={policy} />
										</TableCell>
										<TableCell
											className="px-4"
											onClick={(e) => e.stopPropagation()}
										>
											<DropdownMenu modal={false}>
												<DropdownMenuTrigger asChild>
													<Button
														variant="ghost"
														size="icon"
														data-policy-actions={policy.name}
														aria-label={copy.policies.actionsFor(policy.name)}
													>
														<MoreHorizontal aria-hidden />
													</Button>
												</DropdownMenuTrigger>
												<DropdownMenuContent align="end">
													{policy.hosts === 0 ? (
														<Tooltip>
															<TooltipTrigger asChild>
																<span>
																	<DropdownMenuItem disabled>
																		{copy.policies.runNow}
																	</DropdownMenuItem>
																</span>
															</TooltipTrigger>
															<TooltipContent>
																{copy.policies.runNowDisabled}
															</TooltipContent>
														</Tooltip>
													) : (
														<DropdownMenuItem
															onSelect={() => onRun(policy.name)}
														>
															{copy.policies.runNow}
														</DropdownMenuItem>
													)}
													<DropdownMenuItem
														onSelect={() => void open(policy.name)}
													>
														{copy.policies.edit}
													</DropdownMenuItem>
													<DropdownMenuSeparator />
													<DropdownMenuItem
														variant="destructive"
														onSelect={() => onDelete(policy.name)}
													>
														{copy.policies.delete}
													</DropdownMenuItem>
												</DropdownMenuContent>
											</DropdownMenu>
										</TableCell>
									</TableRow>
								))}
					</TableBody>
				</Table>
			</div>
		</div>
	);
}
