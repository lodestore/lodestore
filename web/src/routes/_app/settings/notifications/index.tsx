import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useRef, useState } from "react";
import { useListChannels } from "@/api/gen/endpoints";
import type { ChannelDto } from "@/api/gen/model";
import { ChannelsTable } from "@/components/settings/notifications/channels-table";
import { DeleteChannelDialog } from "@/components/settings/notifications/delete-channel-dialog";
import { ThresholdsCard } from "@/components/settings/notifications/thresholds-card";
import { SettingsNav } from "@/components/settings/settings-nav";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/settings/notifications/")({
	component: NotificationsPage,
});

function NotificationsPage() {
	const navigate = useNavigate();
	const channels = useListChannels();
	const [deleting, setDeleting] = useState<ChannelDto>();
	const origin = useRef<string>(undefined);
	const add = () => void navigate({ to: "/settings/notifications/new" });

	return (
		<div className="flex flex-col gap-6 pb-8">
			<header className="flex flex-col gap-2">
				<div className="flex items-center justify-between gap-4">
					<h1 data-page-title className="text-2xl font-semibold">
						{copy.notifications.title}
					</h1>
					<Button onClick={add}>{copy.notifications.add}</Button>
				</div>
				<p className="text-sm text-muted-foreground">
					{copy.notifications.lead}
				</p>
			</header>

			<SettingsNav />

			<ChannelsTable
				channels={channels.data}
				isError={channels.isError}
				onRetry={() => channels.refetch()}
				onAdd={add}
				onDelete={(channel) => {
					origin.current = channel.id;
					setDeleting(channel);
				}}
			/>

			<ThresholdsCard />

			<DeleteChannelDialog
				channel={deleting}
				onClose={(deleted) => {
					if (deleted) origin.current = undefined;
					setDeleting(undefined);
				}}
				onCloseAutoFocus={(event) => {
					event.preventDefault();
					const id = origin.current;
					const target =
						id === undefined
							? null
							: document.querySelector<HTMLElement>(
									`[data-channel-actions="${CSS.escape(id)}"]`,
								);
					(
						target ?? document.querySelector<HTMLElement>("[data-page-title]")
					)?.focus();
				}}
			/>
		</div>
	);
}
