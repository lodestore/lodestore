import { createFileRoute } from "@tanstack/react-router";
import { ShieldCheck } from "lucide-react";
import { StubPage } from "@/components/stub-page";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/policies")({
	component: () => <StubPage icon={ShieldCheck} section={copy.nav.policies} />,
});
