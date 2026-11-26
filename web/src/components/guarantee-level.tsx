import { ShieldAlert, ShieldCheck } from "lucide-react";
import { levelName, WEAK_LEVEL } from "@/components/guarantee-notes";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const KEEPS_LEVEL = "overwrite_keeps_old_version";
const CREATE_ONLY_LEVEL = "create_only_enforced";
const ORDER = [WEAK_LEVEL, KEEPS_LEVEL, CREATE_ONLY_LEVEL];
const LABELS = [
	copy.guarantee.scale.weak,
	copy.guarantee.scale.keeps,
	copy.guarantee.scale.createOnly,
];

export function LevelScale({ level }: { level: string }) {
	const measured = ORDER.indexOf(level);
	const tone = measured === 0 ? "bg-warning" : "bg-success";
	return (
		<div className="flex flex-col gap-2">
			<div aria-hidden className="grid grid-cols-3 gap-1">
				{ORDER.map((id, i) => (
					<span
						key={id}
						className={cn("h-2 rounded-sm", i <= measured ? tone : "bg-muted")}
					/>
				))}
			</div>
			<div className="grid grid-cols-3 gap-1 text-sm text-muted-foreground">
				{LABELS.map((label, i) => (
					<span
						key={label}
						className={cn(i === measured && "font-semibold text-foreground")}
					>
						{label}
					</span>
				))}
			</div>
			{measured < 0 && (
				<p className="text-sm text-muted-foreground">
					{copy.guarantee.scale.notMeasured}
				</p>
			)}
		</div>
	);
}

export function LevelLine({
	level,
	warn = false,
}: {
	level: string;
	warn?: boolean;
}) {
	const protective = level === CREATE_ONLY_LEVEL || level === KEEPS_LEVEL;
	const good = protective && !warn;
	const Icon = good ? ShieldCheck : ShieldAlert;
	return (
		<span className="inline-flex items-center gap-2 font-semibold">
			<Icon
				aria-hidden
				className={cn(
					"size-5 shrink-0",
					good ? "text-success" : "text-warning",
				)}
			/>
			<span className={good ? "text-foreground" : "text-warning"}>
				{levelName(level)}
			</span>
		</span>
	);
}
