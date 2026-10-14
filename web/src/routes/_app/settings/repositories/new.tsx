import { createFileRoute } from "@tanstack/react-router";
import { parseStep, type Step } from "@/components/wizard/draft";
import { RepositoryWizard } from "@/components/wizard/repository-wizard";

export const Route = createFileRoute("/_app/settings/repositories/new")({
	validateSearch: (search: Record<string, unknown>): { step?: Step } => {
		const step = parseStep(search.step);
		return step ? { step } : {};
	},
	component: NewRepository,
});

function NewRepository() {
	const { step } = Route.useSearch();
	return <RepositoryWizard mode="settings" step={step} />;
}
