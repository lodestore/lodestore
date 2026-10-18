import { Link, useRouterState } from "@tanstack/react-router";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";

export function SettingsNav() {
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const current = pathname.startsWith("/settings/tokens")
		? "tokens"
		: "repositories";
	return (
		<Tabs value={current}>
			<TabsList variant="line" aria-label={copy.settingsNav.label}>
				<TabsTrigger
					value="repositories"
					asChild
					className="after:bg-primary-strong"
				>
					<Link to="/settings/repositories">
						{copy.settingsNav.repositories}
					</Link>
				</TabsTrigger>
				<TabsTrigger value="tokens" asChild className="after:bg-primary-strong">
					<Link to="/settings/tokens">{copy.settingsNav.tokens}</Link>
				</TabsTrigger>
			</TabsList>
		</Tabs>
	);
}
