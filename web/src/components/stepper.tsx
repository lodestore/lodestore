import { Check } from "lucide-react";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

export type StepperStep = { title: string; description: string };

export function Stepper({
	steps,
	current,
	label = copy.wizard.progressLabel,
}: {
	steps: readonly StepperStep[];
	current: number;
	label?: string;
}) {
	return (
		<ol aria-label={label} className="flex flex-col">
			{steps.map(({ title, description }, i) => {
				const completed = i < current;
				const active = i === current;
				return (
					<li
						key={title}
						aria-current={active ? "step" : undefined}
						data-state={
							completed ? "completed" : active ? "current" : "upcoming"
						}
						className={cn(
							"relative flex h-16 items-center gap-4 rounded-md px-2",
							active && "bg-primary-subtle",
						)}
					>
						{i > 0 && (
							<span
								aria-hidden
								className={cn(
									"absolute bottom-[calc(50%+16px)] left-[calc(0.5rem+15px)] h-[calc(50%-16px)] w-0.5",
									completed || active ? "bg-primary" : "bg-border",
								)}
							/>
						)}
						{i < steps.length - 1 && (
							<span
								aria-hidden
								className={cn(
									"absolute top-[calc(50%+16px)] left-[calc(0.5rem+15px)] h-[calc(50%-16px)] w-0.5",
									completed ? "bg-primary" : "bg-border",
								)}
							/>
						)}
						<span
							className={cn(
								"relative flex size-8 shrink-0 items-center justify-center rounded-full text-sm font-semibold",
								completed && "bg-primary text-primary-foreground",
								active &&
									"border-2 border-primary-strong bg-background text-primary-strong",
								!completed && !active && "bg-muted text-muted-foreground",
							)}
						>
							{completed ? (
								<>
									<Check aria-hidden className="size-4" />
									<span className="sr-only">{copy.wizard.completed}</span>
								</>
							) : (
								i + 1
							)}
						</span>
						<span className="flex min-w-0 flex-col">
							<span
								className={cn(
									"truncate text-base font-semibold",
									!completed && !active && "text-muted-foreground",
								)}
							>
								{title}
							</span>
							<span className="truncate text-sm text-muted-foreground">
								{description}
							</span>
						</span>
					</li>
				);
			})}
		</ol>
	);
}
