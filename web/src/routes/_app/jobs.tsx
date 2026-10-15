import { createFileRoute } from "@tanstack/react-router";
import { ListChecks } from "lucide-react";
import { PreviewPage } from "@/components/preview-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/jobs")({
	component: () => (
		<PreviewPage
			icon={ListChecks}
			{...copy.preview.jobs}
			illustration="preview-jobs"
		/>
	),
});
