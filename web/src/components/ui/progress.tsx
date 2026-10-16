import { Progress as ProgressPrimitive } from "radix-ui";
import type * as React from "react";
import { cn } from "@/lib/utils";

function Progress({
	className,
	value,
	...props
}: React.ComponentProps<typeof ProgressPrimitive.Root>) {
	return (
		<ProgressPrimitive.Root
			data-slot="progress"
			value={value}
			className={cn(
				"relative flex h-2 w-full items-center overflow-x-hidden rounded-full border border-border bg-muted",
				className,
			)}
			{...props}
		>
			<ProgressPrimitive.Indicator
				data-slot="progress-indicator"
				className={cn(
					"size-full flex-1 bg-primary-strong transition-[transform] duration-200 ease-out",
					value == null && "animate-pulse motion-reduce:animate-none",
				)}
				style={{
					transform: value == null ? undefined : `translateX(-${100 - value}%)`,
				}}
			/>
		</ProgressPrimitive.Root>
	);
}

export { Progress };
