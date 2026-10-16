import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, useNavigate } from "@tanstack/react-router";
import { MoreHorizontal } from "lucide-react";
import { toast } from "sonner";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useRecheck,
} from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { PathText } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { AgentKeyBadge } from "@/components/repos/agent-key-badge";
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
import { Spinner } from "@/components/ui/spinner";
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
import { repositoriesQuery } from "@/lib/session";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/_app/settings/repositories/")({
	component: RepositoriesPage,
});

const COLUMNS = Object.entries(copy.repos.columns);
const WIDE_ONLY = new Set(["agentKey", "lastChecked"]);
const wideOnly = (key: string) =>
	WIDE_ONLY.has(key) ? "hidden xl:table-cell" : undefined;

function AddRepositoryButton() {
	return (
		<Button asChild>
			<Link to="/settings/repositories/new">{copy.repos.add}</Link>
		</Button>
	);
}

function Location({ value }: { value: string }) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span
					tabIndex={0}
					className="block max-w-32 truncate font-mono text-sm xl:max-w-sm"
				>
					{value}
				</span>
			</TooltipTrigger>
			<TooltipContent className="max-w-lg">
				<PathText value={value} />
			</TooltipContent>
		</Tooltip>
	);
}

function RowActions({ repository }: { repository: RepositorySummary }) {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const recheck = useRecheck({
		mutation: {
			onError: (e) =>
				toast.error(copy.errors.submit(copy.repos.checkFailed, e.message)),
			onSuccess: () =>
				Promise.all([
					queryClient.invalidateQueries({
						queryKey: getGetRepositoryQueryKey(repository.id),
					}),
					queryClient.invalidateQueries({
						queryKey: getListRepositoriesQueryKey(),
					}),
				]),
		},
	});
	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					aria-label={copy.repos.actionsFor(repository.name)}
				>
					{recheck.isPending ? <Spinner /> : <MoreHorizontal aria-hidden />}
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem
					onSelect={() =>
						void navigate({
							to: "/settings/repositories/$id",
							params: { id: repository.id },
						})
					}
				>
					{copy.repos.viewRepository}
				</DropdownMenuItem>
				<DropdownMenuSeparator />
				<DropdownMenuItem
					disabled={recheck.isPending}
					onSelect={() => recheck.mutate({ id: repository.id })}
				>
					{copy.repos.checkAgain}
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

function RepositoriesPage() {
	const query = useQuery({ ...repositoriesQuery, staleTime: 0 });
	const rows = query.data;

	return (
		<div className="flex flex-col gap-6">
			<header className="flex items-center justify-between gap-4">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.repos.title}
				</h1>
				<AddRepositoryButton />
			</header>

			{query.isError && (
				<Alert variant="destructive">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.errors.list("repositories")}
						<Button
							type="button"
							variant="secondary"
							size="sm"
							onClick={() => query.refetch()}
						>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}

			{rows?.length === 0 ? (
				<Empty className="py-24">
					<Illustration
						name="repos-empty"
						width={320}
						height={216}
						className="h-auto w-80 max-w-full"
					/>
					<EmptyHeader>
						<EmptyTitle>{copy.repos.emptyHeading}</EmptyTitle>
						<EmptyDescription>{copy.repos.emptyBody}</EmptyDescription>
					</EmptyHeader>
					<EmptyContent>
						<AddRepositoryButton />
					</EmptyContent>
				</Empty>
			) : (
				(rows !== undefined || !query.isError) && (
					<div className="overflow-hidden rounded-lg border bg-card">
						<Table>
							<TableHeader className="bg-muted">
								<TableRow>
									{COLUMNS.map(([key, name]) => (
										<TableHead key={key} className={cn("px-4", wideOnly(key))}>
											{name}
										</TableHead>
									))}
									<TableHead className="px-4">
										<span className="sr-only">{copy.repos.actionsColumn}</span>
									</TableHead>
								</TableRow>
							</TableHeader>
							<TableBody>
								{rows === undefined
									? [0, 1, 2, 3, 4].map((n) => (
											<TableRow
												key={n}
												data-testid="skeleton-row"
												className="h-16"
											>
												{[...COLUMNS, ["actions", ""]].map(([key]) => (
													<TableCell
														key={key}
														className={cn("px-4", wideOnly(key))}
													>
														<Skeleton className="h-5 w-full" />
													</TableCell>
												))}
											</TableRow>
										))
									: rows.map((repository) => (
											<TableRow key={repository.id} className="h-16">
												<TableCell className="px-4">
													<Link
														to="/settings/repositories/$id"
														params={{ id: repository.id }}
														className="rounded-sm text-base font-semibold"
													>
														{repository.name}
													</Link>
												</TableCell>
												<TableCell className="px-4">
													<div className="flex gap-1">
														<Badge variant="secondary">
															{repository.backend}
														</Badge>
														{repository.experimental && (
															<Badge variant="outline">
																{copy.backend.experimental}
															</Badge>
														)}
													</div>
												</TableCell>
												<TableCell className="px-4">
													<Location value={repository.location_display} />
												</TableCell>
												<TableCell className="px-4">
													{repository.hosts_count}
												</TableCell>
												<TableCell className={cn("px-4", wideOnly("agentKey"))}>
													{repository.backend === "local" ? (
														copy.common.dash
													) : (
														<AgentKeyBadge state={repository.agent_key_state} />
													)}
												</TableCell>
												<TableCell
													className={cn("px-4", wideOnly("lastChecked"))}
												>
													{repository.checked_at ? (
														<RelativeTime
															value={new Date(repository.checked_at)}
														/>
													) : (
														copy.common.dash
													)}
												</TableCell>
												<TableCell className="px-4">
													<RowActions repository={repository} />
												</TableCell>
											</TableRow>
										))}
							</TableBody>
						</Table>
					</div>
				)
			)}
		</div>
	);
}
