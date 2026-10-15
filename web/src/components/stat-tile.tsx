import { Card } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";

export function StatTile({
	label,
	value,
	isPending,
}: {
	label: string;
	value: number;
	isPending: boolean;
}) {
	return (
		<Card className="flex-row items-center justify-between gap-4 p-4">
			<div className="text-sm text-muted-foreground">{label}</div>
			{isPending ? (
				<Skeleton className="h-6 w-12" />
			) : (
				<div className="text-xl font-semibold tabular-nums">{value}</div>
			)}
		</Card>
	);
}
