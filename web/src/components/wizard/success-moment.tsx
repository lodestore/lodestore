import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { copy } from "@/copy";

export function SuccessMoment({
	hostname,
	version,
}: {
	hostname: string;
	version: string;
}) {
	const [drawn, setDrawn] = useState(false);
	useEffect(() => {
		const frame = requestAnimationFrame(() => setDrawn(true));
		return () => cancelAnimationFrame(frame);
	}, []);

	return (
		<div
			role="status"
			className="flex flex-col items-start gap-4 animate-in fade-in duration-200"
		>
			<div className="flex size-16 items-center justify-center rounded-full bg-success/10">
				<svg
					aria-hidden
					viewBox="0 0 24 24"
					className="size-8 text-success"
					fill="none"
					stroke="currentColor"
					strokeWidth={2.5}
					strokeLinecap="round"
					strokeLinejoin="round"
				>
					<path
						d="M5 12.5l4.5 4.5L19 7.5"
						pathLength={1}
						strokeDasharray={1}
						strokeDashoffset={drawn ? 0 : 1}
						className="transition-[stroke-dashoffset] duration-400"
						data-drawn={drawn}
					/>
				</svg>
			</div>
			<p className="text-display font-semibold break-words">
				{copy.firstHost.successHeading(hostname)}
			</p>
			<p className="text-base text-muted-foreground">
				{copy.firstHost.successDetail(version)}
			</p>
			<Link
				to="/policies/new"
				className="text-sm font-semibold text-primary-strong underline-offset-4 hover:underline"
			>
				{copy.firstHost.nextStep}
			</Link>
		</div>
	);
}
