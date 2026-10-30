import type { AgentUpdateStatus } from "@/api/gen/model";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const text = copy.agentUpdate.confirm;

function Row({ label, value }: { label: string; value: string }) {
	return (
		<>
			<dt className="text-muted-foreground">{label}</dt>
			<dd>{value}</dd>
		</>
	);
}

export function UpdateConfirmDialog({
	open,
	onOpenChange,
	status,
	count,
	offline,
	host,
	pending,
	onConfirm,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	status: AgentUpdateStatus;
	count: number;
	offline: number;
	host?: string;
	pending: boolean;
	onConfirm: () => void;
}) {
	const version = status.target_version ?? "";
	const { settings } = status;
	return (
		<AlertDialog
			open={open}
			onOpenChange={(next) => {
				if (!pending) onOpenChange(next);
			}}
		>
			<AlertDialogContent className="sm:max-w-140">
				<AlertDialogHeader>
					<AlertDialogTitle>
						{host === undefined
							? text.title(count, version)
							: text.titleOne(host, version)}
					</AlertDialogTitle>
				</AlertDialogHeader>
				<dl className="grid grid-cols-[136px_1fr] gap-x-4 gap-y-2 text-sm">
					{host === undefined && (
						<>
							<Row
								label={text.first}
								value={text.firstValue(settings.canary, settings.wave)}
							/>
							<Row
								label={text.pauseAfter}
								value={text.pauseAfterValue(settings.pause_after)}
							/>
						</>
					)}
					<Row label={text.busy} value={text.busyValue} />
					{offline > 0 && (
						<Row label={text.offline} value={text.offlineValue(offline)} />
					)}
				</dl>
				<AlertDialogDescription>
					{text.note(settings.rollback_after_secs)}
				</AlertDialogDescription>
				<AlertDialogFooter>
					<AlertDialogCancel disabled={pending}>
						{text.cancel}
					</AlertDialogCancel>
					<AlertDialogAction
						disabled={pending}
						onClick={(event) => {
							event.preventDefault();
							onConfirm();
						}}
					>
						{pending && <Spinner />}
						{host === undefined ? text.submit(count) : text.submitOne}
					</AlertDialogAction>
				</AlertDialogFooter>
			</AlertDialogContent>
		</AlertDialog>
	);
}
