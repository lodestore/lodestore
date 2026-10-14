import {
	CircleCheck,
	CircleX,
	Clock,
	Minus,
	TriangleAlert,
} from "lucide-react";
import type { ReactNode } from "react";
import type { CheckDto } from "@/api/gen/model";
import {
	Item,
	ItemActions,
	ItemContent,
	ItemDescription,
	ItemGroup,
	ItemMedia,
	ItemTitle,
} from "@/components/ui/item";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

export const CHECK_IDS = [
	"write",
	"list",
	"conditional_write",
	"agent_delete",
	"object_lock",
] as const;

export type CheckId = (typeof CHECK_IDS)[number];
export type CheckStatus =
	| "pass"
	| "warn"
	| "fail"
	| "neutral"
	| "running"
	| "stale";

export type CheckRow = {
	id: CheckId;
	status: CheckStatus;
	detail?: ReactNode;
};

type CheckSource = { checks: CheckDto[]; agent_key_state: string };

const NOT_APPLICABLE = "not_applicable";
const NOT_SUPPORTED = "not_supported";
const STATUSES: CheckStatus[] = ["pass", "warn", "fail", "neutral"];

const capitalize = (text: string) =>
	text.charAt(0).toUpperCase() + text.slice(1);

export function agentKeyWarning(
	source: CheckSource,
): { heading: string; body: string } | undefined {
	const text = copy.check.detail;
	const state = source.agent_key_state;
	if (state !== "can_delete" && state !== "can_overwrite") return undefined;
	const lockOn =
		source.checks.find((c) => c.id === "object_lock")?.status === "pass";
	return {
		heading:
			state === "can_delete"
				? text.agentKeyCanDeleteHeading
				: text.agentKeyCanOverwriteHeading,
		body: [
			state === "can_delete"
				? text.agentKeyCanDeleteBody
				: text.agentKeyCanOverwriteBody,
			lockOn ? text.agentKeyLockOn : text.agentKeyLockOff,
			text.agentKeyAccept,
		].join(" "),
	};
}

function detailFor(
	check: CheckDto,
	source: CheckSource,
	backend: string,
	brief: boolean,
): ReactNode {
	const text = copy.check.detail;
	const error = check.error ?? "";
	if (check.status === "fail") {
		return text.fail(capitalize(error.replace(/\.$/, "")) || copy.common.dash);
	}
	if (check.status === "pass") {
		return {
			write: text.writePass,
			list: text.listPass,
			conditional_write: text.conditionalPass,
			agent_delete: text.agentKeyPass,
			object_lock: text.objectLockOn,
		}[check.id as CheckId];
	}
	if (
		check.id === "conditional_write" &&
		check.status === "warn" &&
		(error === "unsupported" || error.startsWith("a second write"))
	) {
		return text.conditionalWarn;
	}
	if (check.id === "agent_delete") {
		const warning =
			check.status === "warn" ? agentKeyWarning(source) : undefined;
		if (warning && brief) return text.agentKeyBriefRow;
		if (warning) {
			return (
				<>
					<strong className="block font-semibold text-foreground">
						{warning.heading}
					</strong>
					{warning.body}
				</>
			);
		}
		if (check.status === "neutral" && backend === "local") {
			return text.localDelete;
		}
	}
	if (check.id === "object_lock") {
		if (error === NOT_APPLICABLE) return text.objectLockNotApplicable;
		if (error === NOT_SUPPORTED) return text.objectLockNotSupported;
		if (error === "" && check.status === "neutral") return text.objectLockOff;
	}
	return error === "" ? undefined : capitalize(error);
}

export function toCheckRows(
	source: CheckSource,
	backend: string,
	options: { brief?: boolean } = {},
): CheckRow[] {
	return source.checks.flatMap((check) => {
		const status = check.status as CheckStatus;
		if (!CHECK_IDS.includes(check.id as CheckId)) return [];
		if (!STATUSES.includes(status)) return [];
		return [
			{
				id: check.id as CheckId,
				status,
				detail: detailFor(check, source, backend, options.brief ?? false),
			},
		];
	});
}

const visuals = {
	pass: { icon: CircleCheck, tone: "text-success" },
	warn: { icon: TriangleAlert, tone: "text-warning" },
	fail: { icon: CircleX, tone: "text-destructive" },
	neutral: { icon: Minus, tone: "text-neutral" },
	stale: { icon: Clock, tone: "text-neutral" },
} as const;

export function CapabilityChecklist({ rows }: { rows: CheckRow[] }) {
	return (
		<div aria-live="polite">
			<ItemGroup className="gap-2">
				{CHECK_IDS.map((id) => {
					const row: CheckRow = rows.find((r) => r.id === id) ?? {
						id,
						status: "neutral",
					};
					const visual = row.status === "running" ? null : visuals[row.status];
					return (
						<Item
							key={id}
							role="listitem"
							variant="outline"
							size="sm"
							data-status={row.status}
							className={cn(row.status === "stale" && "opacity-60")}
						>
							<ItemMedia variant="icon">
								{visual ? (
									<visual.icon aria-hidden className={visual.tone} />
								) : (
									<Spinner />
								)}
							</ItemMedia>
							<ItemContent>
								<ItemTitle>{copy.check.rows[id]}</ItemTitle>
								<ItemDescription>
									{visual ? row.detail : copy.check.rowRunning}
								</ItemDescription>
							</ItemContent>
							{visual && row.status !== "running" && (
								<ItemActions className={cn("text-xs", visual.tone)}>
									{copy.check.status[row.status]}
								</ItemActions>
							)}
						</Item>
					);
				})}
			</ItemGroup>
		</div>
	);
}
