import { useIsMutating, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useId, useState } from "react";
import { toast } from "sonner";
import {
	getCreateDownloadMutationKey,
	getListJobsQueryKey,
	useCreateDownload,
	useListRestoreHosts,
} from "@/api/gen/endpoints";
import type { ArchiveFormat, RestoreSnapshot } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { archiveName } from "@/lib/snapshot";

const TOAST_MS = 8000;
const text = copy.restore.downloadDialog;
const FORMATS = Object.keys(text.formats) as ArchiveFormat[];

/**
 * Hands the archive URL to the browser's download manager without leaving the
 * app. An attachment never renders, so a `load` with content means the server
 * answered with an error body (used or expired ticket, storage unavailable).
 * A `load` without a readable document means the browser showed its own error
 * page instead (server unreachable, frame refused).
 */
// ponytail: frames of successful downloads stay in the DOM until the page reloads.
export function startDownload(
	url: string,
	onError: (message?: string) => void,
) {
	const frame = document.createElement("iframe");
	frame.hidden = true;
	frame.addEventListener("load", () => {
		const doc = frame.contentDocument;
		const body = doc?.body?.textContent?.trim();
		frame.remove();
		if (doc && !body) return;
		let message: string | undefined;
		try {
			message = (JSON.parse(body ?? "") as { message?: string }).message;
		} catch {}
		onError(message);
	});
	frame.src = url;
	document.body.append(frame);
}

function DownloadForm({
	snapshot,
	items,
	onDone,
}: {
	snapshot: RestoreSnapshot;
	items: string[];
	onDone: () => void;
}) {
	const id = useId();
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const [format, setFormat] = useState<ArchiveFormat>("tar_gz");
	const unverified = snapshot.verified_by == null;
	const [confirmed, setConfirmed] = useState(false);
	const download = useCreateDownload();
	const hosts = useListRestoreHosts().data;
	const hostName =
		hosts?.find((host) => host.key === snapshot.host_key)?.hostname ??
		snapshot.hostname;
	const fileName = archiveName(hostName, snapshot.id, text.formats[format].ext);

	function start(event: React.FormEvent) {
		event.preventDefault();
		download.mutate(
			{ data: { snapshot_id: snapshot.id, items, format } },
			{
				onSuccess: ({ id: jobId, url }) => {
					const started = toast.success(text.started, {
						duration: TOAST_MS,
						action: {
							label: copy.restore.viewJob,
							onClick: () =>
								void navigate({ to: "/jobs/$id", params: { id: jobId } }),
						},
					});
					startDownload(url, (message) => {
						toast.dismiss(started);
						toast.error(
							<TextWithPaths text={copy.errors.submit(text.failed, message)} />,
							{
								duration: TOAST_MS,
							},
						);
					});
					void queryClient.invalidateQueries({
						queryKey: getListJobsQueryKey(),
					});
					onDone();
				},
				onError: (error) =>
					toast.error(
						<TextWithPaths
							text={copy.errors.submit(text.failed, error.message)}
						/>,
						{
							duration: TOAST_MS,
						},
					),
			},
		);
	}

	return (
		<form onSubmit={start} className="flex flex-col gap-6">
			<RadioGroup
				aria-label={text.format}
				value={format}
				onValueChange={(value) => setFormat(value as ArchiveFormat)}
				className="gap-4"
			>
				{FORMATS.map((option) => (
					<div key={option} className="flex items-start gap-2">
						<RadioGroupItem
							value={option}
							id={`${id}-${option}`}
							aria-describedby={`${id}-${option}-help`}
						/>
						<div className="flex min-w-0 flex-col gap-1">
							<Label htmlFor={`${id}-${option}`}>
								{text.formats[option].ext}
							</Label>
							<p
								id={`${id}-${option}-help`}
								className="text-sm text-muted-foreground"
							>
								{text.formats[option].help}
							</p>
						</div>
					</div>
				))}
			</RadioGroup>

			{unverified && (
				<div className="flex items-start gap-2">
					<Checkbox
						id={`${id}-unverified`}
						checked={confirmed}
						onCheckedChange={(value) => setConfirmed(value === true)}
						className="mt-px"
					/>
					<Label
						htmlFor={`${id}-unverified`}
						className="leading-snug font-normal"
					>
						{text.unverifiedConfirm}
					</Label>
				</div>
			)}

			<div className="flex flex-col gap-2 text-sm text-muted-foreground">
				<p className="wrap-anywhere">
					{text.fileName} <span className="font-mono">{fileName}</span>
				</p>
				<p>{text.limits}</p>
			</div>

			<DialogFooter>
				<Button
					type="button"
					variant="outline"
					disabled={download.isPending}
					onClick={onDone}
				>
					{text.dismiss}
				</Button>
				<Button
					type="submit"
					disabled={download.isPending || (unverified && !confirmed)}
				>
					{download.isPending && <Spinner />}
					{text.confirm}
				</Button>
			</DialogFooter>
		</form>
	);
}

export function DownloadDialog({
	open,
	onOpenChange,
	snapshot,
	items,
	onCloseAutoFocus,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	snapshot: RestoreSnapshot | undefined;
	items: string[];
	onCloseAutoFocus: (event: Event) => void;
}) {
	const busy =
		useIsMutating({ mutationKey: getCreateDownloadMutationKey() }) > 0;
	return (
		<Dialog
			open={open}
			onOpenChange={(next) => (next || !busy) && onOpenChange(next)}
		>
			<DialogContent
				onCloseAutoFocus={onCloseAutoFocus}
				className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-140"
			>
				<DialogHeader>
					<DialogTitle className="text-2xl">
						{text.title(items.length)}
					</DialogTitle>
					<DialogDescription>{text.body}</DialogDescription>
				</DialogHeader>
				{snapshot && (
					<DownloadForm
						snapshot={snapshot}
						items={items}
						onDone={() => onOpenChange(false)}
					/>
				)}
			</DialogContent>
		</Dialog>
	);
}
