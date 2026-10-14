import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";
import {
	getGetHostQueryKey,
	getListHostsQueryKey,
	useSetLabels,
} from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
import { LabelsEditor } from "@/components/labels-editor";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { type LabelRow, validateLabels } from "@/lib/labels";

function LabelsForm({ host, onDone }: { host: Host; onDone: () => void }) {
	const queryClient = useQueryClient();
	const save = useSetLabels();
	const [rows, setRows] = useState<LabelRow[]>(() =>
		host.labels.map(({ key, value }) => ({ key, value })),
	);
	const { valid } = validateLabels(rows);

	return (
		<form
			className="flex flex-col gap-6"
			onSubmit={(event) => {
				event.preventDefault();
				if (!valid) return;
				save.mutate(
					{ id: host.id, data: { labels: rows.filter((r) => r.key !== "") } },
					{
						onSuccess: () => {
							toast.success(copy.hosts.labelsSaved);
							void queryClient.invalidateQueries({
								queryKey: getListHostsQueryKey(),
							});
							void queryClient.invalidateQueries({
								queryKey: getGetHostQueryKey(host.id),
							});
							onDone();
						},
					},
				);
			}}
		>
			<DialogHeader>
				<DialogTitle>{copy.hosts.editLabels}</DialogTitle>
			</DialogHeader>
			<LabelsEditor value={rows} onChange={setRows} />
			{save.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{copy.errors.submit("save labels", save.error.message)}
					</AlertDescription>
				</Alert>
			)}
			<DialogFooter>
				<Button type="button" variant="secondary" onClick={onDone}>
					{copy.wizard.discardChanges}
				</Button>
				<Button type="submit" disabled={!valid || save.isPending}>
					{save.isPending && <Spinner />}
					{copy.hosts.saveLabels}
				</Button>
			</DialogFooter>
		</form>
	);
}

export function EditLabelsDialog({
	host,
	onClose,
	onCloseAutoFocus,
}: {
	host?: Host;
	onClose: () => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	return (
		<Dialog
			open={host !== undefined}
			onOpenChange={(open) => {
				if (!open) onClose();
			}}
		>
			<DialogContent
				className="sm:max-w-[480px]"
				aria-describedby={undefined}
				onCloseAutoFocus={onCloseAutoFocus}
			>
				{host && <LabelsForm host={host} onDone={onClose} />}
			</DialogContent>
		</Dialog>
	);
}
