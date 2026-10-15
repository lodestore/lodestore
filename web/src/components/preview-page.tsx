import { CircleCheck, Clock, type LucideIcon } from "lucide-react";
import {
	Illustration,
	type IllustrationName,
} from "@/components/brand/illustration";
import { Badge } from "@/components/ui/badge";
import { copy } from "@/copy";

export function PreviewPage({
	icon: Icon,
	title,
	lead,
	bullets = [],
	illustration,
	embedded = false,
}: {
	icon: LucideIcon;
	title: string;
	lead: string;
	bullets?: readonly string[];
	illustration: IllustrationName;
	embedded?: boolean;
}) {
	const badge = (
		<Badge variant="outline">
			<Clock aria-hidden />
			{copy.preview.badge}
		</Badge>
	);

	if (embedded) {
		return (
			<section className="flex flex-col items-center gap-4 py-24 text-center">
				<Illustration
					name={illustration}
					width={320}
					height={216}
					className="h-auto w-80 max-w-full"
				/>
				<h2 className="text-2xl font-semibold">{title}</h2>
				{badge}
				<p className="max-w-[56ch] text-base text-muted-foreground">{lead}</p>
			</section>
		);
	}

	return (
		<section className="grid gap-12 py-24 lg:grid-cols-12 lg:items-center">
			<div className="order-2 flex flex-col items-start gap-6 lg:order-1 lg:col-span-7">
				<Icon aria-hidden className="size-8 text-muted-foreground" />
				{badge}
				<h1
					data-page-title
					tabIndex={-1}
					className="text-2xl font-semibold outline-none lg:text-display"
				>
					{title}
				</h1>
				<p className="max-w-[56ch] text-base text-muted-foreground">{lead}</p>
				<ul className="flex flex-col gap-2">
					{bullets.map((bullet) => (
						<li key={bullet} className="flex items-start gap-2 text-base">
							<CircleCheck
								aria-hidden
								className="mt-1 size-4 shrink-0 text-muted-foreground"
							/>
							{bullet}
						</li>
					))}
				</ul>
				<p className="text-sm text-muted-foreground">{copy.preview.footnote}</p>
			</div>
			<div className="order-1 flex justify-center lg:order-2 lg:col-span-5">
				<Illustration
					name={illustration}
					width={480}
					height={320}
					className="h-auto w-80 max-w-full lg:w-full"
				/>
			</div>
		</section>
	);
}
