import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Illustration } from "@/components/brand/illustration";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { copy } from "@/copy";
import { repositoriesQuery } from "@/lib/session";

export function RetryAlert({
	children,
	onRetry,
}: {
	children: React.ReactNode;
	onRetry: () => void;
}) {
	return (
		<Alert variant="destructive" role="alert">
			<AlertDescription className="flex flex-wrap items-center justify-between gap-4">
				{children}
				<Button type="button" variant="secondary" size="sm" onClick={onRetry}>
					{copy.errors.retry}
				</Button>
			</AlertDescription>
		</Alert>
	);
}

export function NothingToRestore() {
	const repositories = useQuery(repositoriesQuery);
	const hasRepository = (repositories.data ?? []).length > 0;
	return (
		<Empty className="py-24">
			<EmptyHeader>
				<Illustration
					name="preview-restore"
					width={320}
					height={216}
					className="h-auto w-80 max-w-full"
				/>
				<EmptyTitle>{copy.restore.empty.heading}</EmptyTitle>
				<EmptyDescription>{copy.restore.empty.body}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button asChild>
					{hasRepository || repositories.isPending ? (
						<Link to="/policies/new">{copy.restore.empty.createPolicy}</Link>
					) : (
						<Link to="/settings/repositories/new">
							{copy.restore.empty.addRepository}
						</Link>
					)}
				</Button>
			</EmptyContent>
		</Empty>
	);
}
