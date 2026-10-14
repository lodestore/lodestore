import { Check } from "lucide-react";
import { cn } from "@/lib/utils";

export function Stepper({
	steps,
	current,
	label = "Progress",
}: {
	steps: readonly string[];
	current: number;
	label?: string;
}) {
	return (
		<ol aria-label={label} className="flex w-full">
			{steps.map((name, i) => {
				const completed = i < current;
				const active = i === current;
				return (
					<li
						key={name}
						aria-current={active ? "step" : undefined}
						data-state={
							completed ? "completed" : active ? "current" : "upcoming"
						}
						className="relative flex flex-1 flex-col items-center gap-1"
					>
						{i > 0 && (
							<span
								aria-hidden
								className={cn(
									"absolute top-3 right-[calc(50%+16px)] left-[calc(-50%+16px)] h-0.5",
									i <= current ? "bg-primary" : "bg-border",
								)}
							/>
						)}
						<span
							className={cn(
								"relative flex size-6 items-center justify-center rounded-full border text-xs font-semibold",
								completed && "border-primary bg-background text-primary",
								active && "border-primary bg-primary text-primary-foreground",
								!completed && !active && "bg-muted text-muted-foreground",
							)}
						>
							{completed ? <Check aria-hidden className="size-3" /> : i + 1}
						</span>
						<span
							className={cn(
								"text-xs",
								active ? "font-semibold" : "text-muted-foreground",
							)}
						>
							{name}
						</span>
					</li>
				);
			})}
		</ol>
	);
}
