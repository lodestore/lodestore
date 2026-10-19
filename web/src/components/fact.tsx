export function Fact({
	label,
	children,
}: {
	label: string;
	children: React.ReactNode;
}) {
	return (
		<div className="flex flex-col gap-1">
			<dt className="text-sm text-muted-foreground">{label}</dt>
			<dd className="font-semibold">{children}</dd>
		</div>
	);
}
