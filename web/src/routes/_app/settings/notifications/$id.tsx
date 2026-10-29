import { createFileRoute } from "@tanstack/react-router";
import { useGetChannel } from "@/api/gen/endpoints";
import {
	BackToNotifications,
	ChannelForm,
	ChannelPageShell,
	ConfigChannelView,
} from "@/components/settings/notifications/channel-form";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/settings/notifications/$id")({
	component: ChannelPage,
});

function ChannelPage() {
	const { id } = Route.useParams();
	const query = useGetChannel(id);
	const channel = query.data;
	const title = channel?.name ?? copy.notifications.title;

	if (query.error?.status === 404) {
		return (
			<ChannelPageShell
				crumb={copy.notifications.form.notFound}
				title={copy.notifications.title}
			>
				<Empty>
					<EmptyHeader>
						<EmptyTitle>{copy.notifications.form.notFound}</EmptyTitle>
					</EmptyHeader>
					<EmptyContent>
						<BackToNotifications />
					</EmptyContent>
				</Empty>
			</ChannelPageShell>
		);
	}

	return (
		<ChannelPageShell crumb={title} title={title}>
			{query.isError ? (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{copy.notifications.form.loadFailed}
						<Button variant="outline" size="sm" onClick={() => query.refetch()}>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			) : channel?.source === "config" ? (
				<ConfigChannelView channel={channel} />
			) : channel ? (
				<ChannelForm key={channel.id} channel={channel} />
			) : (
				<div className="flex flex-col gap-6" data-testid="form-skeleton">
					<Skeleton className="h-10 w-full" />
					<Skeleton className="h-10 w-full" />
					<Skeleton className="h-10 w-full" />
				</div>
			)}
		</ChannelPageShell>
	);
}
