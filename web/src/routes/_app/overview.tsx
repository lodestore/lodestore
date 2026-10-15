import { createFileRoute } from "@tanstack/react-router";
import { LayoutDashboard } from "lucide-react";
import { PreviewPage } from "@/components/preview-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/overview")({
	component: () => (
		<PreviewPage
			icon={LayoutDashboard}
			{...copy.preview.overview}
			illustration="preview-overview"
		/>
	),
});
