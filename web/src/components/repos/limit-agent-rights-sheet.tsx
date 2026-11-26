import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert } from "lucide-react";
import { type ReactNode, useState } from "react";
import { toast } from "sonner";
import { ApiError, requestStepUp } from "@/api/fetcher";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	getStorageAccessQueryKey,
	useGetRepository,
	useReplaceAgentOptions,
	useReplaceKey,
	useStorageAccess,
	useStoragePresets,
} from "@/api/gen/endpoints";
import type { StorageAccess } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Sheet,
	SheetClose,
	SheetContent,
	SheetDescription,
	SheetFooter,
	SheetHeader,
	SheetTitle,
	SheetTrigger,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import {
	AgentRightsTasks,
	Commands,
} from "@/components/wizard/agent-rights-tasks";
import {
	type AgentSecrets,
	agentFilled,
	agentUserOf,
	type Backend,
	emptyAgent,
	presetUsesServerKey,
	toAgentPayload,
} from "@/components/wizard/draft";
import { copy } from "@/copy";
import { offlineAgents, onlineAgents } from "@/lib/plural";
import { presetOf } from "@/lib/presets";

export type LimitTarget = "agent-key" | "agent-options";

export function limitTarget(
	access: Pick<StorageAccess, "rotate_via" | "after_revoke"> | undefined,
): LimitTarget | null {
	if (access?.rotate_via === "agent-key") return "agent-key";
	if (access?.rotate_via === "agent-options") return "agent-options";
	return access?.after_revoke === "server_key_shared" ? "agent-key" : null;
}

type Kind = keyof typeof copy.limit.lead;
type Replaced = {
	online_notified: number;
	offline: number;
	needs_update?: number;
};

function Body({
	repositoryId,
	onClose,
}: {
	repositoryId: string;
	onClose: () => void;
}) {
	const queryClient = useQueryClient();
	const repository = useGetRepository(repositoryId);
	const presets = useStoragePresets();
	const access = useStorageAccess(repositoryId);
	const [agent, setAgent] = useState<AgentSecrets>(emptyAgent);
	const [busy, setBusy] = useState(false);
	const [error, setError] = useState<string>();
	const replaceKey = useReplaceKey();
	const replaceOptions = useReplaceAgentOptions();

	const loading = repository.isPending || presets.isPending || access.isPending;
	const failed = repository.isError || presets.isError || access.isError;
	const preset =
		repository.data && presets.data
			? presetOf(repository.data, presets.data)
			: undefined;
	const options = repository.data?.location.options ?? {};
	const bucket = options.bucket ?? "";
	const prefix = options.prefix ?? options.root ?? "";
	const target = limitTarget(access.data);
	const instructionOnly = preset !== undefined && presetUsesServerKey(preset);
	const kind: Kind | undefined = !preset
		? undefined
		: instructionOnly
			? "r2"
			: (preset.backend as Kind);
	const ready = preset !== undefined && agentFilled(preset, agent);

	async function send(): Promise<Replaced | undefined> {
		if (!preset || !target) return;
		const payload = toAgentPayload(preset.backend as Backend, {
			...agent,
			user: agentUserOf(preset, agent),
		});
		if (target === "agent-key") {
			const credentials = payload.agent_credentials;
			if (!credentials) return;
			return replaceKey.mutateAsync({ id: repositoryId, data: credentials });
		}
		const agentOptions = payload.agent_options;
		if (!agentOptions) return;
		return replaceOptions.mutateAsync({
			id: repositoryId,
			data: { agent_options: agentOptions },
		});
	}

	async function submit() {
		if (!preset || busy) return;
		setError(undefined);
		setBusy(true);
		try {
			if (!(await requestStepUp())) return;
			const shared = access.data?.after_revoke === "server_key_shared";
			const result = await send();
			if (!result) return;
			const needsUpdate = result.needs_update ?? 0;
			const sent = copy.repos.replaceKeyResult(
				onlineAgents(result.online_notified),
				offlineAgents(result.offline),
			);
			toast.success(
				shared ? copy.limit.doneShared : copy.limit.done(preset.label),
				{
					description:
						needsUpdate > 0
							? `${sent} ${copy.limit.needsUpdate(needsUpdate)}`
							: sent,
				},
			);
			await Promise.all([
				queryClient.invalidateQueries({
					queryKey: getGetRepositoryQueryKey(repositoryId),
				}),
				queryClient.invalidateQueries({
					queryKey: getListRepositoriesQueryKey(),
				}),
				queryClient.invalidateQueries({
					queryKey: getStorageAccessQueryKey(repositoryId),
				}),
			]);
			onClose();
		} catch (e) {
			if (e instanceof ApiError && e.code === "step_up_required") return;
			setError(
				copy.errors.submit(
					copy.limit.failed,
					e instanceof Error ? e.message : undefined,
				),
			);
		} finally {
			setBusy(false);
		}
	}

	return (
		<>
			<SheetHeader>
				<SheetTitle>{copy.limit.title}</SheetTitle>
				{kind && <SheetDescription>{copy.limit.lead[kind]}</SheetDescription>}
			</SheetHeader>
			<div className="flex min-h-0 flex-1 flex-col gap-6 overflow-y-auto px-4">
				{loading && <Skeleton className="h-40 w-full" />}
				{failed && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertDescription>{copy.repos.loadFailed}</AlertDescription>
					</Alert>
				)}
				{preset &&
					(instructionOnly ? (
						<Commands
							preset={preset}
							bucket={bucket}
							prefix={prefix}
							user=""
							variant="sheet"
						/>
					) : (
						<AgentRightsTasks
							preset={preset}
							bucket={bucket}
							prefix={prefix}
							agent={agent}
							onAgent={setAgent}
							disabled={busy}
							variant="sheet"
						/>
					))}
				{error && (
					<Alert variant="destructive" role="alert">
						<CircleAlert aria-hidden />
						<AlertDescription>{error}</AlertDescription>
					</Alert>
				)}
			</div>
			<SheetFooter className="flex-row justify-end">
				{instructionOnly ? (
					<SheetClose asChild>
						<Button type="button" variant="outline">
							{copy.limit.close}
						</Button>
					</SheetClose>
				) : (
					<>
						<SheetClose asChild>
							<Button type="button" variant="outline" disabled={busy}>
								{copy.limit.cancel}
							</Button>
						</SheetClose>
						<Button
							type="button"
							disabled={!ready || !target || busy}
							onClick={() => void submit()}
						>
							{busy && <Spinner />}
							{busy ? copy.limit.checking : copy.limit.submit}
						</Button>
					</>
				)}
			</SheetFooter>
		</>
	);
}

export function LimitAgentRightsSheet({
	repositoryId,
	trigger,
	open,
	onOpenChange,
}: {
	repositoryId: string;
	trigger?: ReactNode;
	open?: boolean;
	onOpenChange?: (open: boolean) => void;
}) {
	const [inner, setInner] = useState(false);
	const shown = open ?? inner;
	const change = (next: boolean) => {
		setInner(next);
		onOpenChange?.(next);
	};
	return (
		<Sheet open={shown} onOpenChange={change}>
			{trigger && <SheetTrigger asChild>{trigger}</SheetTrigger>}
			<SheetContent
				aria-describedby={undefined}
				className="data-[side=right]:w-full data-[side=right]:sm:max-w-none data-[side=right]:md:max-w-[480px]"
			>
				{shown && (
					<Body repositoryId={repositoryId} onClose={() => change(false)} />
				)}
			</SheetContent>
		</Sheet>
	);
}
