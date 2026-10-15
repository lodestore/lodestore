import type { ReactNode } from "react";
import { AppBanners } from "@/components/app-banners";
import { LogoMark } from "@/components/brand/logo-mark";
import { Stepper, type StepperStep } from "@/components/stepper";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

export type WizardLayoutMode = "welcome" | "settings";

function Brand({ className }: { className?: string }) {
	return (
		<div
			className={cn(
				"flex items-center gap-2 text-base font-semibold",
				className,
			)}
		>
			<LogoMark className="size-7 shrink-0" />
			<span>{copy.brand}</span>
		</div>
	);
}

export function WizardLayout({
	mode,
	steps,
	current,
	wide = false,
	children,
}: {
	mode: WizardLayoutMode;
	steps: readonly StepperStep[];
	current: number;
	wide?: boolean;
	children: ReactNode;
}) {
	const progress = copy.wizard.progress(current + 1, steps.length);
	const name = steps[current]?.title ?? "";
	return (
		<div className="grid min-h-svh lg:h-svh lg:grid-cols-[320px_minmax(0,1fr)] lg:grid-rows-[minmax(0,1fr)]">
			<aside className="hidden flex-col gap-6 overflow-y-auto bg-sidebar p-8 lg:flex">
				<Brand />
				<Stepper steps={steps} current={current} />
				<p className="text-sm text-muted-foreground">{progress}</p>
				{mode === "welcome" && (
					<p className="mt-auto pt-6 text-sm text-muted-foreground [@media(max-height:720px)]:hidden">
						{copy.wizard.railFootnote}
					</p>
				)}
			</aside>
			<header className="sticky top-0 z-10 bg-background lg:hidden">
				<div className="flex h-16 items-center gap-4 border-b px-6">
					<LogoMark className="size-7 shrink-0" />
					<p className="min-w-0 truncate text-sm font-semibold">
						{`${progress}: ${name}`}
					</p>
				</div>
				<div
					role="progressbar"
					aria-label={copy.wizard.progressLabel}
					aria-valuemin={0}
					aria-valuemax={steps.length}
					aria-valuenow={current + 1}
					className="h-1 bg-muted"
				>
					<div
						className="h-full bg-primary-strong"
						style={{ width: `${((current + 1) / steps.length) * 100}%` }}
					/>
				</div>
			</header>
			<main
				id="main"
				tabIndex={-1}
				className="relative flex min-w-0 flex-col outline-none lg:overflow-y-auto"
			>
				<div
					className={cn(
						"mx-auto flex w-full max-w-[720px] flex-1 flex-col px-6 pt-8 pb-8 lg:mx-0 lg:px-12 lg:py-12 xl:px-16",
						wide && "max-w-[1040px]",
					)}
				>
					<AppBanners />
					{children}
				</div>
			</main>
		</div>
	);
}
