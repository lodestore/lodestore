import { createFileRoute, Outlet } from "@tanstack/react-router";
import { Spinner } from "@/components/ui/spinner";
import { useFocusHeading } from "@/lib/focus-heading";
import { requireSession } from "@/lib/session";

export const Route = createFileRoute("/_wizard")({
	beforeLoad: ({ context: { queryClient }, location }) =>
		requireSession(queryClient, location),
	pendingMs: 200,
	pendingMinMs: 0,
	pendingComponent: () => (
		<div className="flex min-h-svh items-center justify-center">
			<Spinner className="size-6" />
		</div>
	),
	component: WizardRoute,
});

function WizardRoute() {
	useFocusHeading();
	return <Outlet />;
}
