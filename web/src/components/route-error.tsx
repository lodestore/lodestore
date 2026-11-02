import { type ErrorComponentProps, useRouter } from "@tanstack/react-router";
import { ApiError } from "@/api/fetcher";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

export function RouteError({ error }: ErrorComponentProps) {
	const router = useRouter();
	const message =
		error instanceof ApiError && error.status === 429
			? copy.login.rateLimited(error.retryAfter ?? 60)
			: copy.errors.page;
	return (
		<div className="flex min-h-svh items-center justify-center p-6">
			<Alert variant="destructive" className="max-w-lg">
				<AlertDescription className="flex items-center justify-between gap-4">
					{message}
					<Button
						type="button"
						variant="secondary"
						size="sm"
						onClick={() => router.invalidate()}
					>
						{copy.errors.retry}
					</Button>
				</AlertDescription>
			</Alert>
		</div>
	);
}
