import { createFileRoute } from "@tanstack/react-router";
import { ShieldCheck } from "lucide-react";
import { PreviewPage } from "@/components/preview-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/policies")({
	component: () => (
		<PreviewPage
			icon={ShieldCheck}
			{...copy.preview.policies}
			illustration="preview-policies"
		/>
	),
});
