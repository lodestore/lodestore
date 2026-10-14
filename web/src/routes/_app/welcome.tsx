import { createFileRoute } from "@tanstack/react-router";
import { parseStep, type Step } from "@/components/wizard/draft";
import { RepositoryWizard } from "@/components/wizard/repository-wizard";

export const Route = createFileRoute("/_app/welcome")({
	validateSearch: (search: Record<string, unknown>): { step?: Step } => {
		const step = parseStep(search.step);
		return step ? { step } : {};
	},
	component: Welcome,
});

function Welcome() {
	const { step } = Route.useSearch();
	return <RepositoryWizard mode="welcome" step={step} />;
}
