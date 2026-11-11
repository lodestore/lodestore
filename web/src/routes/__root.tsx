import { type QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createRootRouteWithContext, Outlet } from "@tanstack/react-router";
import { StepUpDialog } from "@/components/step-up-dialog";
import { ThemeProvider } from "@/components/theme/theme-provider";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";

export const Route = createRootRouteWithContext<{ queryClient: QueryClient }>()(
	{
		component: RootLayout,
	},
);

function RootLayout() {
	const { queryClient } = Route.useRouteContext();
	return (
		<QueryClientProvider client={queryClient}>
			<ThemeProvider>
				<TooltipProvider>
					<Outlet />
					<StepUpDialog />
					<Toaster
						position="bottom-right"
						duration={4000}
						offset={{ bottom: 88 }}
						mobileOffset={{ bottom: 80 }}
					/>
				</TooltipProvider>
			</ThemeProvider>
		</QueryClientProvider>
	);
}
