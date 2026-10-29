import { useQueryClient } from "@tanstack/react-query";
import { BellOff } from "lucide-react";
import { useEffect, useRef } from "react";
import { toast } from "sonner";
import {
	getCountAlertsQueryKey,
	getFleetHostQueryKey,
	getFleetOverviewQueryKey,
	getListAlertsQueryKey,
	useResumeHostAlerts,
	useSilenceHost,
} from "@/api/gen/endpoints";
import { SilenceDuration } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const TOAST_MS = 4_000;
const text = copy.overview.alerts.silence;
const durations = Object.values(SilenceDuration);

export function useSilence(hostId: string, hostname: string) {
	const queryClient = useQueryClient();
	const silenceHost = useSilenceHost();
	const resumeHost = useResumeHostAlerts();

	const refresh = () => {
		for (const queryKey of [
			getFleetOverviewQueryKey(),
			getFleetHostQueryKey(hostId),
			getListAlertsQueryKey(),
			getCountAlertsQueryKey(),
		]) {
			void queryClient.invalidateQueries({ queryKey });
		}
	};

	return {
		pending: silenceHost.isPending || resumeHost.isPending,
		silence: (duration: SilenceDuration) =>
			silenceHost.mutate(
				{ id: hostId, data: { duration } },
				{
					onSuccess: () => {
						toast.success(text.silenced(hostname, duration), {
							duration: TOAST_MS,
						});
						refresh();
					},
					onError: (error) =>
						toast.error(
							copy.errors.submit(text.failed(hostname), error.message),
						),
				},
			),
		resume: () =>
			resumeHost.mutate(
				{ id: hostId },
				{
					onSuccess: () => {
						toast.success(text.resumed(hostname), { duration: TOAST_MS });
						refresh();
					},
					onError: (error) =>
						toast.error(
							copy.errors.submit(text.resumeFailed(hostname), error.message),
						),
				},
			),
	};
}

export function SilenceItems({
	silenced,
	onSilence,
	onResume,
}: {
	silenced: boolean;
	onSilence: (duration: SilenceDuration) => void;
	onResume: () => void;
}) {
	return (
		<>
			{silenced && (
				<>
					<DropdownMenuItem onSelect={onResume}>{text.resume}</DropdownMenuItem>
					<DropdownMenuSeparator />
				</>
			)}
			{durations.map((duration) => (
				<DropdownMenuItem key={duration} onSelect={() => onSilence(duration)}>
					{text.durations[duration]}
				</DropdownMenuItem>
			))}
		</>
	);
}

export function SilenceMenu({
	hostId,
	hostname,
	silenced,
}: {
	hostId: string;
	hostname: string;
	silenced: boolean;
}) {
	const { pending, silence, resume } = useSilence(hostId, hostname);
	const trigger = useRef<HTMLButtonElement>(null);
	const wasPending = useRef(false);

	useEffect(() => {
		if (wasPending.current && !pending) trigger.current?.focus();
		wasPending.current = pending;
	}, [pending]);

	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					ref={trigger}
					variant="outline"
					disabled={pending}
					aria-label={text.triggerFor(hostname)}
				>
					{pending ? <Spinner /> : <BellOff aria-hidden />}
					{text.trigger}
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<SilenceItems
					silenced={silenced}
					onSilence={silence}
					onResume={resume}
				/>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}
