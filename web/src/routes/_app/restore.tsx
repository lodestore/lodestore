import { createFileRoute } from "@tanstack/react-router";
import { ArchiveRestore } from "lucide-react";
import { PreviewPage } from "@/components/preview-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/restore")({
	component: () => (
		<PreviewPage
			icon={ArchiveRestore}
			{...copy.preview.restore}
			illustration="preview-restore"
		/>
	),
});
