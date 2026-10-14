import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Database } from "lucide-react";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useRecheck,
} from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { RelativeTime } from "@/components/relative-time";
import { AgentKeyBadge } from "@/components/repos/agent-key-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
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
	TooltipProvider,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { repositoriesQuery } from "@/lib/session";

export const Route = createFileRoute("/_app/settings/repositories/")({
	component: RepositoriesPage,
});

const COLUMNS = Object.values(copy.repos.columns);

function AddRepositoryButton() {
	return (
		<Button asChild>
			<Link to="/settings/repositories/new">{copy.repos.add}</Link>
		</Button>
	);
}

function Location({ value }: { value: string }) {
	return (
		<TooltipProvider>
			<Tooltip>
				<TooltipTrigger asChild>
					<span
						tabIndex={0}
						className="block max-w-[28ch] truncate font-mono text-sm"
					>
						{value}
					</span>
				</TooltipTrigger>
				<TooltipContent className="break-all">{value}</TooltipContent>
			</Tooltip>
		</TooltipProvider>
	);
}

function RowActions({ repository }: { repository: RepositorySummary }) {
	const queryClient = useQueryClient();
	const recheck = useRecheck({
		mutation: {
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
		<div className="flex justify-end gap-2">
			<Button asChild variant="ghost" size="sm">
				<Link to="/settings/repositories/$id" params={{ id: repository.id }}>
					{copy.repos.viewRepository}
				</Link>
			</Button>
			<Button
				type="button"
				variant="ghost"
				size="sm"
				disabled={recheck.isPending}
				onClick={() => recheck.mutate({ id: repository.id })}
			>
				{recheck.isPending && <Spinner />}
				{copy.repos.checkAgain}
			</Button>
		</div>
	);
}

function RepositoriesPage() {
	const query = useQuery({ ...repositoriesQuery, staleTime: 0 });
	const rows = query.data;

	return (
		<div className="flex flex-col gap-6">
			<header className="flex items-center justify-between gap-4">
				<h1 className="text-2xl font-semibold">{copy.repos.title}</h1>
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
				<Empty>
					<EmptyHeader>
						<EmptyMedia variant="icon">
							<Database />
						</EmptyMedia>
						<EmptyTitle>{copy.repos.emptyHeading}</EmptyTitle>
						<EmptyDescription>{copy.repos.emptyBody}</EmptyDescription>
					</EmptyHeader>
					<EmptyContent>
						<AddRepositoryButton />
					</EmptyContent>
				</Empty>
			) : (
				(rows !== undefined || !query.isError) && (
					<Table>
						<TableHeader>
							<TableRow>
								{COLUMNS.map((name) => (
									<TableHead key={name}>{name}</TableHead>
								))}
								<TableHead>
									<span className="sr-only">{copy.repos.actionsColumn}</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{rows === undefined
								? [0, 1, 2, 3, 4].map((n) => (
										<TableRow key={n} data-testid="skeleton-row">
											{[...COLUMNS, ""].map((name) => (
												<TableCell key={name}>
													<Skeleton className="h-5 w-full" />
												</TableCell>
											))}
										</TableRow>
									))
								: rows.map((repository) => (
										<TableRow key={repository.id}>
											<TableCell className="font-semibold">
												{repository.name}
											</TableCell>
											<TableCell>
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
											<TableCell>
												<Location value={repository.location_display} />
											</TableCell>
											<TableCell>{repository.hosts_count}</TableCell>
											<TableCell>
												<AgentKeyBadge state={repository.agent_key_state} />
											</TableCell>
											<TableCell>
												{repository.checked_at ? (
													<RelativeTime
														value={new Date(repository.checked_at)}
													/>
												) : (
													copy.common.dash
												)}
											</TableCell>
											<TableCell>
												<RowActions repository={repository} />
											</TableCell>
										</TableRow>
									))}
						</TableBody>
					</Table>
				)
			)}
		</div>
	);
}
