import { Link, useRouterState } from "@tanstack/react-router";
import { useEffect, useRef } from "react";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";

function activeTab(pathname: string) {
	if (pathname.startsWith("/settings/user")) return "user";
	if (pathname.startsWith("/settings/tokens")) return "tokens";
	if (pathname.startsWith("/settings/notifications")) return "notifications";
	return "repositories";
}

export function SettingsNav() {
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const current = activeTab(pathname);
	const list = useRef<HTMLDivElement>(null);

	useEffect(() => {
		list.current
			?.querySelector("[data-state=active]")
			?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
	}, [current]);

	return (
		<Tabs value={current}>
			<TabsList
				ref={list}
				variant="line"
				aria-label={copy.settingsNav.label}
				className="max-w-full justify-start overflow-x-auto pb-2"
			>
				<TabsTrigger
					value="repositories"
					asChild
					className="after:bg-primary-strong"
				>
					<Link to="/settings/repositories">
						{copy.settingsNav.repositories}
					</Link>
				</TabsTrigger>
				<TabsTrigger
					value="notifications"
					asChild
					className="after:bg-primary-strong"
				>
					<Link to="/settings/notifications">
						{copy.settingsNav.notifications}
					</Link>
				</TabsTrigger>
				<TabsTrigger value="tokens" asChild className="after:bg-primary-strong">
					<Link to="/settings/tokens">{copy.settingsNav.tokens}</Link>
				</TabsTrigger>
				<TabsTrigger value="user" asChild className="after:bg-primary-strong">
					<Link to="/settings/user">{copy.settingsNav.user}</Link>
				</TabsTrigger>
			</TabsList>
		</Tabs>
	);
}
