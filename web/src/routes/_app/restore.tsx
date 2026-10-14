import { createFileRoute } from "@tanstack/react-router";
import { ArchiveRestore } from "lucide-react";
import { StubPage } from "@/components/stub-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/restore")({
	component: () => (
		<StubPage icon={ArchiveRestore} section={copy.nav.restore} />
	),
});
