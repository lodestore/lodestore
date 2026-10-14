import { createFileRoute } from "@tanstack/react-router";
import { LayoutDashboard } from "lucide-react";
import { StubPage } from "@/components/stub-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/overview")({
	component: () => (
		<StubPage icon={LayoutDashboard} section={copy.nav.overview} />
	),
});
