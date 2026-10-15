import { createFileRoute, Outlet, redirect } from "@tanstack/react-router";
import { Search } from "lucide-react";
import { useEffect, useState } from "react";
import { AppBanners } from "@/components/app-banners";
import { AppSidebar } from "@/components/app-sidebar";
import { CommandPalette } from "@/components/command-palette";
import { Button } from "@/components/ui/button";
import { ModK } from "@/components/ui/kbd";
import { SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { useFocusHeading } from "@/lib/focus-heading";
import { infoQuery, repositoriesQuery, requireSession } from "@/lib/session";

export const Route = createFileRoute("/_app")({
	beforeLoad: async ({ context: { queryClient }, location }) => {
		await requireSession(queryClient, location);
		const [, repositories] = await Promise.all([
			queryClient.ensureQueryData(infoQuery),
			queryClient.fetchQuery(repositoriesQuery),
		]);
		if (repositories.length === 0) {
			throw redirect({ to: "/welcome" });
		}
	},
	pendingMs: 200,
	pendingMinMs: 0,
	pendingComponent: PendingShell,
	component: AppLayout,
});

function PendingShell() {
	const [spinning, setSpinning] = useState(true);
	useEffect(() => {
		const timer = setTimeout(() => setSpinning(false), 1000);
		return () => clearTimeout(timer);
	}, []);

	if (spinning) {
		return (
			<div className="flex min-h-svh items-center justify-center">
				<Spinner className="size-6" />
			</div>
		);
	}
	return (
		<div className="flex min-h-svh">
			<Skeleton className="hidden w-64 rounded-none md:block" />
			<div className="flex-1 space-y-4 p-6">
				<Skeleton className="h-8 w-48" />
				<Skeleton className="h-64 w-full" />
			</div>
		</div>
	);
}

function sidebarDefaultOpen() {
	const saved = /(?:^|;\s*)sidebar_state=(true|false)/.exec(
		document.cookie,
	)?.[1];
	return saved ? saved === "true" : window.innerWidth >= 1024;
}

function AppLayout() {
	useFocusHeading();
	const [paletteOpen, setPaletteOpen] = useState(false);
	useEffect(() => {
		const onKeyDown = (event: KeyboardEvent) => {
			if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
				event.preventDefault();
				setPaletteOpen((open) => !open);
			}
		};
		window.addEventListener("keydown", onKeyDown);
		return () => window.removeEventListener("keydown", onKeyDown);
	}, []);
	return (
		<SidebarProvider defaultOpen={sidebarDefaultOpen()}>
			<a
				href="#main"
				className="sr-only rounded-md bg-primary px-4 py-2 text-base font-semibold text-primary-foreground focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-50"
			>
				{copy.shell.skipLink}
			</a>
			<AppSidebar onOpenPalette={() => setPaletteOpen(true)} />
			<main id="main" tabIndex={-1} className="min-w-0 flex-1 outline-none">
				<header className="sticky top-0 z-10 flex h-16 items-center justify-between border-b bg-background px-6 xl:px-8">
					<SidebarTrigger
						aria-label={copy.shell.toggleSidebar}
						className="lg:group-has-data-[state=expanded]/sidebar-wrapper:invisible"
					/>
					<Button variant="outline" onClick={() => setPaletteOpen(true)}>
						<Search aria-hidden />
						{copy.shell.searchTrigger}
						<ModK className="ml-4" />
					</Button>
				</header>
				<div className="mx-auto max-w-7xl px-6 pt-8 pb-12 xl:px-8">
					<AppBanners />
					<Outlet />
				</div>
			</main>
			<CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} />
		</SidebarProvider>
	);
}
