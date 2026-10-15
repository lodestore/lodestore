import { createFileRoute, Outlet } from "@tanstack/react-router";
import { AuthLayout } from "@/components/layout/auth-layout";
import { useFocusHeading } from "@/lib/focus-heading";

export const Route = createFileRoute("/_auth")({
	component: AuthRoute,
});

function AuthRoute() {
	useFocusHeading();
	return (
		<AuthLayout>
			<Outlet />
		</AuthLayout>
	);
}
