import { keepPreviousData } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Search } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { z } from "zod";
import { useListJobs, useListPolicies } from "@/api/gen/endpoints";
import type { ListJobsParams } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import {
	CancelJobDialog,
	useRerunAction,
} from "@/components/jobs/cancel-job-dialog";
import {
	type JobRow,
	JobsTable,
	jobsRefetchInterval,
	toJobRow,
} from "@/components/jobs/jobs-table";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Input } from "@/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { copy } from "@/copy";

const STATES = [
	"queued",
	"running",
	"succeeded",
	"warnings",
	"failed",
	"cancelled",
	"interrupted",
	"not_started",
] as const;

const KINDS = ["backup", "restore", "download"] as const;

const PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 300;
const ALL = "all";
const CLI_POLICY = "(cli)";

const text = z
	.union([z.string(), z.number()])
	.transform(String)
	.optional()
	.catch(undefined);

export const Route = createFileRoute("/_app/jobs/")({
	validateSearch: z.object({
		status: z.enum(STATES).optional().catch(undefined),
		kind: z.enum(KINDS).optional().catch(undefined),
		policy: text,
		host: text,
		page: z.number().int().min(1).optional().catch(undefined),
	}),
	component: JobsPage,
});

function JobsPage() {
	const { status, kind, policy, host, page = 1 } = Route.useSearch();
	const navigate = Route.useNavigate();
	const [cancelling, setCancelling] = useState<JobRow>();
	const rerun = useRerunAction();
	const originId = useRef<string>(undefined);

	const params: ListJobsParams = {
		status,
		kind,
		policy: policy === CLI_POLICY ? undefined : policy,
		source: policy === CLI_POLICY ? "cli" : undefined,
		host: host || undefined,
		page: page > 1 ? page : undefined,
	};
	const jobs = useListJobs(params, {
		query: {
			refetchInterval: jobsRefetchInterval,
			refetchIntervalInBackground: false,
			placeholderData: keepPreviousData,
		},
	});
	const policies = useListPolicies();

	const pushed = useRef(host ?? "");
	const [query, setQuery] = useState(host ?? "");
	useEffect(() => {
		if ((host ?? "") === pushed.current) return;
		pushed.current = host ?? "";
		setQuery(host ?? "");
	}, [host]);
	useEffect(() => {
		const wanted = query.trim();
		if (wanted === pushed.current) return;
		const timer = setTimeout(() => {
			pushed.current = wanted;
			void navigate({
				search: (prev) => ({
					...prev,
					host: wanted || undefined,
					page: undefined,
				}),
				replace: true,
			});
		}, SEARCH_DEBOUNCE_MS);
		return () => clearTimeout(timer);
	}, [query, navigate]);

	const filtered = Boolean(status || kind || policy || host);
	const setFilter = (key: "status" | "kind" | "policy", value: string) =>
		void navigate({
			search: (prev) => ({
				...prev,
				[key]: value === ALL ? undefined : value,
				page: undefined,
			}),
		});
	const setPage = (next: number) =>
		void navigate({
			search: (prev) => ({ ...prev, page: next > 1 ? next : undefined }),
		});

	const data = jobs.data;
	const total = data?.total ?? 0;
	const lastPage = Math.max(1, Math.ceil(total / PAGE_SIZE));
	const offLast = data !== undefined && total > 0 && page > lastPage;
	useEffect(() => {
		if (offLast && !jobs.isPlaceholderData) {
			void navigate({
				search: (prev) => ({
					...prev,
					page: lastPage > 1 ? lastPage : undefined,
				}),
				replace: true,
			});
		}
	}, [offLast, lastPage, jobs.isPlaceholderData, navigate]);

	const policyNames = new Set((policies.data ?? []).map((p) => p.name));
	if (policy && policy !== CLI_POLICY) policyNames.add(policy);

	function restoreFocus(event: Event) {
		const id = originId.current;
		if (id === undefined) return;
		event.preventDefault();
		document
			.querySelector<HTMLElement>(`[data-job-actions="${CSS.escape(id)}"]`)
			?.focus();
	}

	const noJobsAtAll = data !== undefined && total === 0 && !filtered;
	let body: React.ReactNode;
	if (data === undefined) {
		body = jobs.isPending ? <JobsTable jobs={[]} loading /> : null;
	} else if (noJobsAtAll) {
		body = (
			<Empty className="py-24">
				<EmptyHeader>
					<Illustration
						name="preview-jobs"
						width={320}
						height={216}
						className="h-auto w-80 max-w-full"
					/>
					<EmptyTitle>{copy.jobs.emptyHeading}</EmptyTitle>
					<EmptyDescription>{copy.jobs.emptyBody}</EmptyDescription>
				</EmptyHeader>
				<EmptyContent>
					<Button asChild>
						<Link to="/policies/new">{copy.palette.createPolicy}</Link>
					</Button>
				</EmptyContent>
			</Empty>
		);
	} else if (data.items.length === 0 && !offLast) {
		body = (
			<Empty>
				<EmptyHeader>
					<EmptyTitle>{copy.jobs.filteredEmptyHeading}</EmptyTitle>
				</EmptyHeader>
				<EmptyContent>
					<Button
						variant="outline"
						onClick={() => {
							setQuery("");
							void navigate({ search: {} });
						}}
					>
						{copy.jobs.clearFilters}
					</Button>
				</EmptyContent>
			</Empty>
		);
	} else {
		body = (
			<JobsTable
				jobs={data.items.map(toJobRow)}
				onCancel={(job) => {
					originId.current = job.id;
					setCancelling(job);
				}}
				onRerun={(job) => {
					originId.current = job.id;
					rerun.run(job);
				}}
			/>
		);
	}

	const from = (page - 1) * PAGE_SIZE + 1;

	return (
		<div className="flex flex-col gap-6 pb-8">
			<h1 data-page-title className="text-2xl font-semibold">
				{copy.jobs.title}
			</h1>
			<div className="flex flex-col gap-4">
				{!noJobsAtAll && (
					<div className="flex flex-wrap items-center gap-4">
						<div className="relative min-w-56 max-w-90 flex-1">
							<Search
								aria-hidden
								className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
							/>
							<Input
								type="search"
								aria-label={copy.jobs.searchLabel}
								placeholder={copy.jobs.searchPlaceholder}
								value={query}
								className="pl-10"
								onChange={(e) => setQuery(e.target.value)}
							/>
						</div>
						<Select
							value={status ?? ALL}
							onValueChange={(value) => setFilter("status", value)}
						>
							<SelectTrigger
								aria-label={copy.jobs.statusFilterLabel}
								className="data-[size=default]:h-10 w-44"
							>
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								<SelectItem value={ALL}>{copy.jobs.allStatuses}</SelectItem>
								{STATES.map((state) => (
									<SelectItem key={state} value={state}>
										{copy.jobs.status[state]}
									</SelectItem>
								))}
							</SelectContent>
						</Select>
						<Select
							value={kind ?? ALL}
							onValueChange={(value) => setFilter("kind", value)}
						>
							<SelectTrigger
								aria-label={copy.jobs.kindFilterLabel}
								className="data-[size=default]:h-10 w-44"
							>
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								<SelectItem value={ALL}>{copy.jobs.allTypes}</SelectItem>
								{KINDS.map((k) => (
									<SelectItem key={k} value={k}>
										{copy.jobs.kinds[k]}
									</SelectItem>
								))}
							</SelectContent>
						</Select>
						<Select
							value={policy ?? ALL}
							onValueChange={(value) => setFilter("policy", value)}
						>
							<SelectTrigger
								aria-label={copy.jobs.policyFilterLabel}
								className="data-[size=default]:h-10 w-44"
							>
								<SelectValue />
							</SelectTrigger>
							<SelectContent>
								<SelectItem value={ALL}>{copy.jobs.allPolicies}</SelectItem>
								{[...policyNames].sort().map((name) => (
									<SelectItem key={name} value={name}>
										{name}
									</SelectItem>
								))}
								<SelectItem value={CLI_POLICY}>
									{copy.jobs.cliPolicy}
								</SelectItem>
							</SelectContent>
						</Select>
					</div>
				)}

				{jobs.isError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription className="flex items-center justify-between gap-4">
							{copy.jobs.loadFailed}
							<Button
								type="button"
								variant="secondary"
								size="sm"
								onClick={() => jobs.refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				)}

				{body}

				{data !== undefined && data.items.length > 0 && (
					<div className="flex items-center justify-between">
						<p className="text-sm text-muted-foreground">
							{copy.jobs.showing(from, from + data.items.length - 1, total)}
						</p>
						<div className="flex gap-2">
							<Button
								variant="outline"
								size="sm"
								disabled={page <= 1}
								onClick={() => setPage(page - 1)}
							>
								{copy.jobs.previous}
							</Button>
							<Button
								variant="outline"
								size="sm"
								disabled={page >= lastPage}
								onClick={() => setPage(page + 1)}
							>
								{copy.jobs.next}
							</Button>
						</div>
					</div>
				)}
			</div>
			<CancelJobDialog
				job={cancelling}
				onClose={() => setCancelling(undefined)}
				onCloseAutoFocus={restoreFocus}
			/>
		</div>
	);
}
