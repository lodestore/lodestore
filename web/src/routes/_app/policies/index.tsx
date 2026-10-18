import { createFileRoute, Link } from "@tanstack/react-router";
import { useRef, useState } from "react";
import { useListPolicies, useListRepositories } from "@/api/gen/endpoints";
import { DeletePolicyDialog } from "@/components/policies/delete-policy-dialog";
import { PoliciesTable } from "@/components/policies/policies-table";
import { RunNowDialog } from "@/components/policies/run-now-dialog";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/policies/")({
	component: PoliciesPage,
});

const REFRESH_MS = 15_000;

function PoliciesPage() {
	const menuOrigin = useRef<string>(undefined);
	const [running, setRunning] = useState<string>();
	const [deleting, setDeleting] = useState<string>();
	const policies = useListPolicies({
		query: { refetchInterval: REFRESH_MS, refetchIntervalInBackground: false },
	});
	const repositories = useListRepositories();

	function restoreFocus(event: Event) {
		const name = menuOrigin.current;
		if (name === undefined) return;
		event.preventDefault();
		document
			.querySelector<HTMLElement>(`[data-policy-actions="${CSS.escape(name)}"]`)
			?.focus();
	}

	return (
		<div className="flex flex-col gap-6 pb-8">
			<div className="flex items-center justify-between gap-4">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.policies.title}
				</h1>
				<Button asChild>
					<Link to="/policies/new">{copy.policies.create}</Link>
				</Button>
			</div>
			<PoliciesTable
				policies={policies.data}
				isPending={policies.isPending}
				isError={policies.isError}
				onRetry={() => void policies.refetch()}
				hasRepositories={(repositories.data?.length ?? 1) > 0}
				onRun={(name) => {
					menuOrigin.current = name;
					setRunning(name);
				}}
				onDelete={(name) => {
					menuOrigin.current = name;
					setDeleting(name);
				}}
			/>
			<RunNowDialog
				name={running}
				onClose={() => setRunning(undefined)}
				onCloseAutoFocus={restoreFocus}
			/>
			<DeletePolicyDialog
				name={deleting}
				managedBy={
					policies.data?.find((policy) => policy.name === deleting)?.managed_by
				}
				onClose={() => setDeleting(undefined)}
				onCloseAutoFocus={restoreFocus}
			/>
		</div>
	);
}
