import { createFileRoute, Link } from "@tanstack/react-router";
import { useMemo } from "react";
import { z } from "zod";
import { useFleetOverview, useListAlerts } from "@/api/gen/endpoints";
import { type HostState, HostState as HostStates } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import {
	AlertAnnouncer,
	OtherAlerts,
} from "@/components/overview/active-alerts";
import { FleetSummary } from "@/components/overview/fleet-summary";
import { FleetTable } from "@/components/overview/fleet-table";
import { RelativeTime } from "@/components/relative-time";
import { Alert, AlertDescription } from "@/components/ui/alert";
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

export const Route = createFileRoute("/_app/overview")({
	validateSearch: z.object({
		state: z.enum(HostStates).optional().catch(undefined),
	}),
	component: OverviewPage,
});

const REFRESH_MS = 10_000;
const ALERT_LIMIT = 500;
const text = copy.overview;

function OverviewPage() {
	const fleet = useFleetOverview({
		query: { refetchInterval: REFRESH_MS, refetchIntervalInBackground: false },
	});
	const alerts = useListAlerts(
		{ state: "active", limit: ALERT_LIMIT },
		{
			query: {
				refetchInterval: REFRESH_MS,
				refetchIntervalInBackground: false,
			},
		},
	);

	const { data } = fleet;
	const alertList = alerts.data?.alerts;
	const ids = useMemo(() => alertList?.map((a) => a.id), [alertList]);
	const hostless = useMemo(
		() => alertList?.filter((a) => !a.host) ?? [],
		[alertList],
	);
	const { state: filter } = Route.useSearch();
	const navigate = Route.useNavigate();
	const setFilter = (state: HostState | undefined) =>
		void navigate({ search: state ? { state } : {}, replace: true });

	return (
		<div className="@container flex flex-col gap-6 pb-8">
			<h1 data-page-title className="text-2xl font-semibold">
				{text.title}
			</h1>
			{data && fleet.isError && (
				<p className="text-sm text-muted-foreground">
					{text.staleData[0]}
					<RelativeTime value={new Date(fleet.dataUpdatedAt)} />
					{text.staleData[1]}
				</p>
			)}
			{fleet.isPending && (
				<>
					<div className="flex flex-col gap-4">
						<Skeleton className="h-8 w-80 max-w-full" />
						<Skeleton className="h-2 w-full" />
						<Skeleton className="h-8 w-64 max-w-full" />
					</div>
					<FleetTable onClearFilter={() => setFilter(undefined)} />
				</>
			)}
			{fleet.isError && !data && (
				<Alert variant="destructive">
					<AlertDescription className="flex flex-wrap items-center gap-4">
						{text.loadFailed}
						<Button variant="outline" size="sm" onClick={() => fleet.refetch()}>
							{text.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}
			{data &&
				(data.totals.hosts === 0 ? (
					<>
						<OtherAlerts alerts={hostless} />
						<Empty className="py-24">
							<EmptyHeader>
								<Illustration
									name="hosts-empty"
									width={320}
									height={216}
									className="h-auto w-80 max-w-full"
								/>
								<EmptyTitle>{text.empty.heading}</EmptyTitle>
								<EmptyDescription>{text.empty.body}</EmptyDescription>
							</EmptyHeader>
							<EmptyContent>
								<Button asChild>
									<Link to="/hosts" search={{ add: true }}>
										{text.empty.action}
									</Link>
								</Button>
							</EmptyContent>
						</Empty>
					</>
				) : (
					<>
						<FleetSummary
							totals={data.totals}
							storage={data.storage}
							hosts={data.hosts}
							filter={filter}
							onFilter={setFilter}
						/>
						<AlertAnnouncer ids={ids} />

						<OtherAlerts alerts={hostless} />
						{alerts.data && alerts.data.total > alerts.data.alerts.length && (
							<p className="text-sm text-muted-foreground">
								{text.alerts.partial(
									alerts.data.alerts.length,
									alerts.data.total,
								)}
							</p>
						)}
						<FleetTable
							hosts={data.hosts}
							alerts={alertList}
							filter={filter}
							onClearFilter={() => setFilter(undefined)}
						/>
					</>
				))}
		</div>
	);
}
