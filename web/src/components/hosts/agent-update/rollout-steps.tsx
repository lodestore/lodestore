import { CircleCheck, CircleX, Clock, LoaderCircle, Pause } from "lucide-react";
import type { RolloutState, RolloutWave } from "@/api/gen/model";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const text = copy.agentUpdate;

type Phase = "done" | "failed" | "skipped" | "current" | "queued";

const resolved = (w: RolloutWave) => w.waiting === 0;

function finished(w: RolloutWave): Phase {
	if (w.updated > 0) return "done";
	return w.rolled_back > 0 ? "failed" : "skipped";
}

function phases(waves: RolloutWave[], state: RolloutState) {
	const active = state === "running" || state === "paused";
	const current = active
		? waves.findIndex((w) => !resolved(w))
		: waves.length - 1;
	return {
		current,
		of: (w: RolloutWave, index: number): Phase =>
			index === current && active
				? "current"
				: resolved(w)
					? finished(w)
					: "queued",
	};
}

function tone(phase: Phase, state: RolloutState) {
	if (phase === "queued") {
		return { icon: Clock, text: "text-neutral", border: "border-border" };
	}
	if (phase === "done") {
		return {
			icon: CircleCheck,
			text: "text-success",
			border: "border-success",
		};
	}
	if (phase === "failed") {
		return {
			icon: CircleX,
			text: "text-destructive",
			border: "border-destructive",
		};
	}
	if (phase === "skipped") {
		return { icon: CircleX, text: "text-neutral", border: "border-border" };
	}
	return state === "paused"
		? { icon: Pause, text: "text-warning", border: "border-warning" }
		: {
				icon: LoaderCircle,
				text: "text-primary-strong",
				border: "border-primary-strong",
			};
}

function detail(w: RolloutWave, phase: Phase) {
	if (phase === "skipped") return text.steps.skipped(w.total);
	if (phase === "queued" && w.waiting === w.total) {
		return text.steps.queued(w.total);
	}
	return w.wave === 0
		? text.steps.canaryDone(w.updated, w.total)
		: text.steps.waveDone(w.updated, w.rolled_back, w.waiting);
}

export function RolloutSteps({
	waves,
	state,
}: {
	waves: RolloutWave[];
	state: RolloutState;
}) {
	const { current, of } = phases(waves, state);
	const indexes = waves.map((_, i) => i);
	const shown =
		waves.length > 3
			? [0, Math.max(current, 1)].filter((v, i, all) => all.indexOf(v) === i)
			: indexes;
	const more = waves.length - shown.length;
	return (
		<ol className="grid grid-cols-1 gap-4 md:grid-cols-3">
			{shown.map((index) => {
				const wave = waves[index];
				const phase = of(wave, index);
				const t = tone(phase, state);
				const Icon = t.icon;
				return (
					<li key={wave.wave} className={cn("border-t-2 pt-4", t.border)}>
						<div className="flex flex-col gap-1">
							<span
								className={cn(
									"flex items-center gap-2 text-sm font-semibold",
									t.text,
								)}
							>
								<Icon
									aria-hidden
									className={cn(
										"size-4 shrink-0",
										Icon === LoaderCircle &&
											"animate-spin motion-reduce:animate-none",
									)}
								/>
								{wave.wave === 0
									? text.steps.canary
									: text.steps.wave(wave.wave)}
							</span>
							<span className="text-sm text-muted-foreground">
								{detail(wave, phase)}
							</span>
						</div>
					</li>
				);
			})}
			{more > 0 && (
				<li className="border-t-2 border-border pt-4">
					<span className="text-sm text-muted-foreground">
						{text.steps.more(more)}
					</span>
				</li>
			)}
		</ol>
	);
}
