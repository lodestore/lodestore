import { getRouteApi, useRouter } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useEffect, useMemo, useRef } from "react";
import { ApiError } from "@/api/fetcher";
import { useListRestoreHosts } from "@/api/gen/endpoints";
import type { FindGroup } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { FindDetail } from "@/components/restore/find/find-detail";
import { FindFilters } from "@/components/restore/find/find-filters";
import { FindForm } from "@/components/restore/find/find-form";
import { FindResultList } from "@/components/restore/find/find-result-list";
import { FindStatus } from "@/components/restore/find/find-status";
import {
	findKey,
	findRequest,
	useFindSession,
} from "@/components/restore/find/use-find-session";
import { NothingToRestore } from "@/components/restore/restore-states";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";

const route = getRouteApi("/_app/restore");

function ResultSkeleton() {
	return (
		<div className="flex flex-col gap-4" aria-hidden>
			<Skeleton className="h-16 w-full" />
			<Skeleton className="h-16 w-full" />
			<Skeleton className="h-16 w-full" />
		</div>
	);
}

const wide = () => window.matchMedia("(min-width: 1024px)").matches;

export function FindFile() {
	const { find, hosts, labels, since, until, file, run } = route.useSearch();
	const navigate = route.useNavigate();
	const router = useRouter();
	const detailRef = useRef<HTMLDivElement>(null);
	const previous = useRef(file);
	const groupsRef = useRef<FindGroup[]>([]);

	useEffect(() => {
		const before = previous.current;
		previous.current = file;
		if (before === file || wide()) return;
		if (file !== undefined) {
			detailRef.current?.querySelector("h2")?.focus();
		} else if (before !== undefined) {
			const group = groupsRef.current.find((g) => findKey(g) === before);
			if (group) document.getElementById(`find-item-${group.index}`)?.focus();
		}
	}, [file]);

	const back = () =>
		router.history.canGoBack()
			? router.history.back()
			: void navigate({
					search: (prev) => ({ ...prev, file: undefined }),
					replace: true,
				});

	const request = useMemo(
		() => findRequest(find, { hosts, labels, since, until }),
		[find, hosts, labels, since, until],
	);
	const { session, groups, stop, error, retry } = useFindSession(request, run);
	groupsRef.current = groups;
	const selected =
		file === undefined
			? groups[0]
			: (groups.find((group) => findKey(group) === file) ??
				(session?.state === "running" ? undefined : groups[0]));
	const inputError =
		error instanceof ApiError && error.status === 422
			? error.errors?.find((problem) => problem.path === "pattern")?.message
			: undefined;
	const filtered =
		Boolean(hosts) ||
		Boolean(labels) ||
		since !== undefined ||
		until !== undefined;
	const restoreHosts = useListRestoreHosts();
	const noSnapshots = restoreHosts.data?.length === 0;

	const search = (pattern: string) =>
		void navigate({
			search: (prev) => ({
				...prev,
				find: pattern,
				file: undefined,
				run: (prev.run ?? 0) + 1,
			}),
		});

	let empty = null;
	if (groups.length === 0) {
		if (noSnapshots) {
			empty = <NothingToRestore />;
		} else if (find === undefined) {
			empty = (
				<Empty className="py-16">
					<EmptyHeader>
						<Illustration
							name="preview-restore"
							width={240}
							height={162}
							className="h-auto w-60 max-w-full"
						/>
						<EmptyTitle>{copy.find.empty.heading}</EmptyTitle>
						<EmptyDescription>{copy.find.empty.body}</EmptyDescription>
					</EmptyHeader>
					<EmptyContent className="flex-row flex-wrap justify-center">
						{copy.find.empty.examples.map((example) => (
							<Button
								key={example}
								type="button"
								variant="outline"
								size="sm"
								className="font-mono"
								onClick={() => search(example)}
							>
								{example}
							</Button>
						))}
					</EmptyContent>
				</Empty>
			);
		} else if (session?.state === "done") {
			empty = (
				<Empty className="py-16">
					<EmptyHeader>
						<EmptyTitle>
							{filtered && session.total === 0
								? copy.find.noSnapshots.heading
								: copy.find.none.heading(find)}
						</EmptyTitle>
						<EmptyDescription>
							{filtered && session.total === 0
								? copy.find.noSnapshots.body
								: copy.find.none.body}
							{session.skipped > 0 && ` ${copy.find.skipped(session.skipped)}.`}
						</EmptyDescription>
					</EmptyHeader>
				</Empty>
			);
		} else if (!session || session.state === "running") {
			empty = error ? null : <ResultSkeleton />;
		}
	}

	return (
		<div className="flex flex-col gap-6">
			<FindForm
				pattern={find}
				error={inputError}
				autoFocus={find === undefined}
				onSearch={search}
			/>
			<FindFilters
				scope={{ hosts, labels, since, until }}
				onChange={(patch) =>
					void navigate({
						search: (prev) => ({ ...prev, ...patch, file: undefined }),
					})
				}
			/>
			{find !== undefined && (
				<FindStatus
					session={session}
					error={inputError ? null : error}
					onStop={stop}
					onRetry={retry}
				/>
			)}
			{empty}
			{groups.length > 0 && (
				<div className="grid gap-6 lg:grid-cols-[360px_minmax(0,1fr)] lg:items-start">
					<FindResultList
						groups={groups}
						selected={selected && findKey(selected)}
						className={file === undefined ? undefined : "hidden lg:flex"}
					/>
					{session && (selected || file !== undefined) && (
						<div
							ref={detailRef}
							className={`min-w-0 flex-col gap-4 ${file === undefined ? "hidden lg:flex" : "flex"}`}
						>
							<Button
								type="button"
								variant="outline"
								size="sm"
								className="self-start lg:hidden"
								onClick={back}
							>
								<ChevronLeft aria-hidden />
								{copy.find.back}
							</Button>
							{selected ? (
								<FindDetail sessionId={session.id} group={selected} />
							) : (
								<Skeleton className="h-48 w-full" aria-hidden />
							)}
						</div>
					)}
				</div>
			)}
		</div>
	);
}
