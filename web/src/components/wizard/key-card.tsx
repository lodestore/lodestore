import { ShieldCheck } from "lucide-react";
import type { ReactNode } from "react";
import { Badge } from "@/components/ui/badge";

export function KeyCard({
	number,
	title,
	role,
	badge,
	children,
}: {
	number: number;
	title: string;
	role: string;
	badge?: string;
	children: ReactNode;
}) {
	return (
		<section className="flex min-w-0 flex-col gap-4 rounded-lg border p-6 xl:row-span-2 xl:grid xl:grid-rows-subgrid">
			<div className="flex items-start gap-4">
				<span
					aria-hidden
					className="flex size-8 shrink-0 items-center justify-center rounded-md bg-muted font-semibold"
				>
					{number}
				</span>
				<div className="flex min-w-0 flex-1 flex-col gap-1">
					<div className="flex flex-wrap items-center gap-2">
						<h3 className="font-semibold">{title}</h3>
						{badge !== undefined && (
							<Badge variant="outline" className="border-success text-success">
								<ShieldCheck aria-hidden />
								{badge}
							</Badge>
						)}
					</div>
					<p className="text-sm text-muted-foreground">{role}</p>
				</div>
			</div>
			{children}
		</section>
	);
}
