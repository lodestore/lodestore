import { createFileRoute } from "@tanstack/react-router";
import { SettingsNav } from "@/components/settings/settings-nav";
import { EmailCard, PasswordCard } from "@/components/settings/user/user-cards";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/settings/user/")({
	component: UserPage,
});

function UserPage() {
	return (
		<div className="flex max-w-[720px] flex-col gap-6 pb-8">
			<header className="flex flex-col gap-2">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.user.title}
				</h1>
				<p className="text-sm text-muted-foreground">{copy.user.lead}</p>
			</header>

			<SettingsNav />

			<EmailCard />
			<PasswordCard />
		</div>
	);
}
