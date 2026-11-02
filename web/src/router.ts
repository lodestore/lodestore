import { QueryClient } from "@tanstack/react-query";
import { createRouter, type RouterHistory } from "@tanstack/react-router";
import { toast } from "sonner";
import { onDemoBlocked, onUnauthorized } from "./api/fetcher";
import { RouteError } from "./components/route-error";
import { copy } from "./copy";
import { routeTree } from "./routeTree.gen";

export function createAppRouter(
	history?: RouterHistory,
	queryClient = new QueryClient(),
) {
	const router = createRouter({
		routeTree,
		history,
		context: { queryClient },
		defaultErrorComponent: RouteError,
	});
	onUnauthorized(() => {
		const { pathname, href } = router.state.location;
		if (pathname === "/login") return;
		queryClient.clear();
		void router.navigate({
			to: "/login",
			search: { next: href, expired: true },
		});
	});
	onDemoBlocked(() => {
		toast(copy.demo.notAvailable, { id: "demo-mode" });
	});
	return router;
}

declare module "@tanstack/react-router" {
	interface Register {
		router: ReturnType<typeof createAppRouter>;
	}
}
