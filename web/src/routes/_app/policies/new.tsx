import { createFileRoute, Link } from "@tanstack/react-router";
import { useListRepositories } from "@/api/gen/endpoints";
import { PolicyEditor } from "@/components/policies/policy-editor";
import {
	BLANK_LABEL,
	type PolicyDraft,
} from "@/components/policies/policy-form";
import {
	Breadcrumb,
	BreadcrumbItem,
	BreadcrumbLink,
	BreadcrumbList,
	BreadcrumbPage,
	BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";

export const Route = createFileRoute("/_app/policies/new")({
	component: NewPolicyPage,
});

function quickstart(repository: string): PolicyDraft {
	return {
		name: "etc-and-home",
		repository,
		hostMode: "labels",
		labels: [BLANK_LABEL],
		hosts: [],
		paths: ["/etc", "/home"],
		exclude: "**/.cache\n**/node_modules",
		oneFileSystem: false,
		excludeCaches: false,
		before: [],
		after: [],
	};
}

function NewPolicyPage() {
	const repositories = useListRepositories();
	const repos = repositories.data;

	return (
		<div className="flex flex-col gap-6 pb-8">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/policies">{copy.policies.title}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>{copy.policies.newTitle}</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>
			<h1 data-page-title className="text-2xl font-semibold">
				{copy.policies.newTitle}
			</h1>
			{repos ? (
				<PolicyEditor
					starter={quickstart(repos.length === 1 ? repos[0].name : "")}
					repositories={repos}
				/>
			) : (
				<Skeleton className="h-96 w-full" />
			)}
		</div>
	);
}
