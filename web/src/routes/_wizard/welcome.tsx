import { createFileRoute, redirect } from "@tanstack/react-router";
import { parseStep, type Step } from "@/components/wizard/draft";
import { RepositoryWizard } from "@/components/wizard/repository-wizard";
import { repositoriesQuery } from "@/lib/session";

export const Route = createFileRoute("/_wizard/welcome")({
	validateSearch: (search: Record<string, unknown>): { step?: Step } => {
		const step = parseStep(search.step);
		return step ? { step } : {};
	},
	beforeLoad: async ({ context: { queryClient }, search }) => {
		if (search.step === "host") return;
		const repositories = await queryClient.ensureQueryData(repositoriesQuery);
		if (repositories.length > 0) throw redirect({ to: "/hosts" });
	},
	component: Welcome,
});

function Welcome() {
	const { step } = Route.useSearch();
	return <RepositoryWizard mode="welcome" step={step} />;
}
