import { useLinkProps } from "@tanstack/react-router";
import { TriangleAlert } from "lucide-react";
import type { FindGroup } from "@/api/gen/model";
import { PathText } from "@/components/path-text";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { copy } from "@/copy";
import { displayName } from "@/lib/names";
import { cn } from "@/lib/utils";
import { formatMonthDay } from "./dates";
import { findKey } from "./use-find-session";

export function FindResultItem({
	group,
	selected,
}: {
	group: FindGroup;
	selected: boolean;
}) {
	const gone = group.gone != null;
	const link = useLinkProps({
		from: "/restore",
		to: "/restore",
		search: (prev) => ({ ...prev, file: findKey(group) }),
		resetScroll: false,
	});
	return (
		<a
			{...link}
			id={`find-item-${group.index}`}
			aria-current={selected ? "true" : undefined}
			className={`flex min-w-0 flex-col gap-1 p-4 outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset ${selected ? "bg-primary-subtle hover:bg-primary-subtle" : ""}`}
		>
			<span className="flex items-start justify-between gap-2">
				<span className="flex min-w-0 flex-wrap items-center gap-2 font-semibold wrap-anywhere">
					{displayName(group.hostname)}
					{!group.agent && (
						<Badge variant="outline">
							{copy.restore.hostPicker.noAgentBadge}
						</Badge>
					)}
				</span>
				<span className="shrink-0 text-sm text-muted-foreground">
					{copy.find.versions(group.versions.length)}
				</span>
			</span>
			<span className="font-mono text-sm">
				<PathText value={displayName(group.path)} />
			</span>
			<span
				className={`flex items-center gap-1 text-sm ${gone ? "text-warning" : "text-muted-foreground"}`}
			>
				{gone && <TriangleAlert aria-hidden className="size-4" />}
				{copy.find.lastSeen(formatMonthDay(group.last_seen_ms))}
			</span>
		</a>
	);
}

export function FindResultList({
	groups,
	selected,
	className,
}: {
	groups: FindGroup[];
	selected: string | undefined;
	className?: string;
}) {
	return (
		<Card className={cn("gap-0 py-0 lg:sticky lg:top-24", className)}>
			<nav
				aria-label={copy.find.list.label}
				className="flex flex-col divide-y divide-border"
			>
				{groups.map((group) => (
					<FindResultItem
						key={group.index}
						group={group}
						selected={findKey(group) === selected}
					/>
				))}
			</nav>
		</Card>
	);
}
