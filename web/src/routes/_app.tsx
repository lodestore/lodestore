import { createFileRoute, Outlet, redirect } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { AppBanners } from "@/components/app-banners";
import { AppSidebar } from "@/components/app-sidebar";
import { SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { useFocusHeading } from "@/lib/focus-heading";
import {
	infoQuery,
	isSignedOut,
	repositoriesQuery,
	sessionQuery,
} from "@/lib/session";

const OPEN_WITHOUT_REPOSITORIES = ["/welcome", "/settings/repositories/new"];

export const Route = createFileRoute("/_app")({
	beforeLoad: async ({ context: { queryClient }, location }) => {
		try {
			await queryClient.ensureQueryData(sessionQuery);
		} catch (error) {
			if (isSignedOut(error)) {
				throw redirect({
					to: "/login",
					search: { next: location.href, expired: undefined },
				});
			}
			throw error;
		}
		const [, repositories] = await Promise.all([
			queryClient.ensureQueryData(infoQuery),
			queryClient.fetchQuery(repositoriesQuery),
		]);
		const path = location.pathname.replace(/\/+$/, "");
		if (
			repositories.length === 0 &&
			!OPEN_WITHOUT_REPOSITORIES.includes(path)
		) {
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

function AppLayout() {
	useFocusHeading();
	return (
		<SidebarProvider defaultOpen={window.innerWidth >= 1024}>
			<AppSidebar />
			<main className="min-w-0 flex-1">
				<div className="mx-auto max-w-[1200px] px-6 pt-8">
					<SidebarTrigger className="mb-4 md:hidden" />
					<AppBanners />
					<Outlet />
				</div>
			</main>
		</SidebarProvider>
	);
}
