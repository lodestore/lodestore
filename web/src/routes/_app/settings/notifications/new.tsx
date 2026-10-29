import { createFileRoute } from "@tanstack/react-router";
import {
	ChannelForm,
	ChannelPageShell,
} from "@/components/settings/notifications/channel-form";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/settings/notifications/new")({
	component: NewChannelPage,
});

function NewChannelPage() {
	return (
		<ChannelPageShell
			crumb={copy.notifications.newBreadcrumb}
			title={copy.notifications.newTitle}
		>
			<ChannelForm />
		</ChannelPageShell>
	);
}
