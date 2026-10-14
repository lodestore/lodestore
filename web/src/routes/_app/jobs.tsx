import { createFileRoute } from "@tanstack/react-router";
import { ListChecks } from "lucide-react";
import { StubPage } from "@/components/stub-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/jobs")({
	component: () => <StubPage icon={ListChecks} section={copy.nav.jobs} />,
});
