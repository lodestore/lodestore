import { useRouterState } from "@tanstack/react-router";
import { CircleAlert } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { TextWithPaths } from "@/components/path-text";
import { CheckLevelItem } from "@/components/repos/checks/check-level-item";
import { CheckSettings } from "@/components/repos/checks/check-settings";
import { StorageFacts } from "@/components/repos/checks/storage-facts";
import {
	checkActive,
	levelFailed,
	useCheckStatus,
} from "@/components/repos/checks/use-check-status";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { ItemGroup } from "@/components/ui/item";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";

export const CHECKS_ANCHOR = "checks";

const text = copy.checks;

export function ChecksCard({ repositoryId }: { repositoryId: string }) {
	const query = useCheckStatus(repositoryId);
	const hash = useRouterState({ select: (s) => s.location.hash });
	const heading = useRef<HTMLHeadingElement>(null);
	const hint = useId();
	const [startError, setStartError] = useState<string | null>(null);
	useEffect(() => {
		if (hash.replace(/^#/, "") !== CHECKS_ANCHOR) return;
		heading.current?.closest("[data-slot=card]")?.scrollIntoView();
		heading.current?.focus({ preventScroll: true });
	}, [hash]);
	const status = query.data;
	const reason =
		status?.blocked?.reason ?? (checkActive(status) ? "check" : undefined);
	const failed = status?.levels.filter(levelFailed) ?? [];

	return (
		<Card id={CHECKS_ANCHOR} className="scroll-mt-20">
			<CardHeader>
				<CardTitle>
					<h2 ref={heading} tabIndex={-1} className="outline-none">
						{text.title}
					</h2>
				</CardTitle>
				<CardDescription>{text.description}</CardDescription>
			</CardHeader>
			<CardContent className="flex flex-col gap-6">
				{status ? (
					<>
						<StorageFacts usage={status.usage} />
						{failed.length > 0 && (
							<Alert variant="destructive">
								<CircleAlert aria-hidden />
								<AlertTitle>
									{failed.map((level) => (
										<div key={level.level}>
											{text.failedAlert.title(text.levels[level.level].name)}
										</div>
									))}
								</AlertTitle>
								<AlertDescription>{text.failedAlert.body}</AlertDescription>
							</Alert>
						)}
						{startError !== null && (
							<Alert variant="destructive">
								<CircleAlert aria-hidden />
								<AlertTitle>{text.startFailedTitle}</AlertTitle>
								<AlertDescription>
									<TextWithPaths
										text={copy.errors.submit(text.startFailed, startError)}
									/>
								</AlertDescription>
							</Alert>
						)}
						<ItemGroup className="gap-4">
							{status.levels.map((level) => (
								<CheckLevelItem
									key={level.level}
									repositoryId={repositoryId}
									level={level}
									coverage={status.coverage}
									blocked={reason !== undefined}
									hintId={hint}
									onStartError={setStartError}
								/>
							))}
						</ItemGroup>
						{reason && (
							<p id={hint} className="text-sm text-muted-foreground">
								{text.blocked[reason]}
							</p>
						)}
						<CheckSettings repositoryId={repositoryId} status={status} />
					</>
				) : query.isError ? (
					<Alert variant="destructive">
						<CircleAlert aria-hidden />
						<AlertTitle>{text.loadFailedTitle}</AlertTitle>
						<AlertDescription className="flex flex-wrap items-center justify-between gap-4">
							{text.loadFailed}
							<Button
								type="button"
								variant="outline"
								size="sm"
								onClick={() => query.refetch()}
							>
								{copy.errors.retry}
							</Button>
						</AlertDescription>
					</Alert>
				) : (
					<>
						<Skeleton className="h-12 w-full" />
						<Skeleton className="h-18 w-full" />
						<Skeleton className="h-18 w-full" />
					</>
				)}
			</CardContent>
		</Card>
	);
}
