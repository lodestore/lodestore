import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { Hash, Lock, Mail, MoreHorizontal, Send, Webhook } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { getListChannelsQueryKey, useTestChannel } from "@/api/gen/endpoints";
import { AlertType, type ChannelDto, type ChannelKind } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyTitle,
} from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import {
	Table,
	TableBody,
	TableCaption,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const text = copy.notifications.table;
const TOAST_MS = 4000;
const ALERT_TYPE_COUNT = Object.values(AlertType).length;
const SHOW_TYPE = "hidden @min-[560px]:table-cell";
const SHOW_ALERTS = "hidden @min-[840px]:table-cell";

export const KIND_ICONS: Record<ChannelKind, typeof Send> = {
	telegram: Send,
	slack: Hash,
	email: Mail,
	webhook: Webhook,
};

export function KindLabel({ kind }: { kind: ChannelKind }) {
	const Icon = KIND_ICONS[kind];
	return (
		<span className="inline-flex items-center gap-2">
			<Icon aria-hidden className="size-4 text-muted-foreground" />
			{copy.notifications.kinds[kind]}
		</span>
	);
}

function AlertTypes({ channel }: { channel: ChannelDto }) {
	const all = channel.alert_types.length === ALERT_TYPE_COUNT;
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span tabIndex={0}>
					{all
						? text.allTypes
						: text.someTypes(channel.alert_types.length, ALERT_TYPE_COUNT)}
				</span>
			</TooltipTrigger>
			<TooltipContent>
				<ul>
					{channel.alert_types.map((type) => (
						<li key={type}>{copy.notifications.alertTypes[type]}</li>
					))}
				</ul>
			</TooltipContent>
		</Tooltip>
	);
}

function LastDelivery({ channel }: { channel: ChannelDto }) {
	const last = channel.last_delivery;
	if (!last)
		return <StatusBadge status="neutral">{text.neverSent}</StatusBadge>;
	const badge = (
		<StatusBadge status={last.ok ? "succeeded" : "failed"}>
			{last.ok ? text.sent : text.failed}
		</StatusBadge>
	);
	return (
		<div className="flex flex-wrap items-center gap-x-2 gap-y-1">
			{last.ok || !last.error ? (
				badge
			) : (
				<Tooltip>
					<TooltipTrigger asChild>
						<span tabIndex={0}>{badge}</span>
					</TooltipTrigger>
					<TooltipContent className="max-w-80 wrap-anywhere">
						<TextWithPaths text={last.error} />
					</TooltipContent>
				</Tooltip>
			)}
			<span className="text-sm text-muted-foreground">
				<RelativeTime value={new Date(last.at)} />
			</span>
		</div>
	);
}

function RowActions({
	channel,
	onDelete,
}: {
	channel: ChannelDto;
	onDelete?: (channel: ChannelDto) => void;
}) {
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const test = useTestChannel();
	const [open, setOpen] = useState(false);
	const config = channel.source === "config";

	function sendTest() {
		test.mutate(
			{ data: { channel_id: channel.id } },
			{
				onSuccess: (result) => {
					if (result.ok) {
						toast.success(copy.notifications.test.toastSent(channel.name), {
							duration: TOAST_MS,
						});
					} else {
						toast.error(
							copy.notifications.test.toastFailed(channel.name, result.message),
						);
					}
				},
				onError: (error) =>
					toast.error(
						copy.notifications.test.toastFailed(channel.name, error.message),
					),
				onSettled: () => {
					setOpen(false);
					void queryClient.invalidateQueries({
						queryKey: getListChannelsQueryKey(),
					});
				},
			},
		);
	}

	return (
		<DropdownMenu modal={false} open={open} onOpenChange={setOpen}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					data-channel-actions={channel.id}
					aria-label={text.actionsFor(channel.name)}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem
					disabled={test.isPending}
					onSelect={(event) => {
						event.preventDefault();
						sendTest();
					}}
				>
					{test.isPending && <Spinner />}
					{text.sendTest}
				</DropdownMenuItem>
				<DropdownMenuItem
					onSelect={() =>
						void navigate({
							to: "/settings/notifications/$id",
							params: { id: channel.id },
						})
					}
				>
					{config ? text.view : text.edit}
				</DropdownMenuItem>
				{!config && onDelete && (
					<DropdownMenuItem
						variant="destructive"
						onSelect={() => onDelete(channel)}
					>
						{text.delete}
					</DropdownMenuItem>
				)}
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

function SkeletonRows() {
	return Array.from({ length: 3 }, (_, i) => (
		<TableRow key={i} aria-hidden data-testid="skeleton-row" className="h-16">
			<TableCell className="px-4">
				<Skeleton className="h-5 w-40" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_TYPE)}>
				<Skeleton className="h-5 w-24" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_ALERTS)}>
				<Skeleton className="h-5 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="h-6 w-32" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="size-10" />
			</TableCell>
		</TableRow>
	));
}

export function ChannelsTable({
	channels,
	isError,
	onRetry,
	onAdd,
	onDelete,
}: {
	channels?: ChannelDto[];
	isError: boolean;
	onRetry: () => void;
	onAdd: () => void;
	onDelete?: (channel: ChannelDto) => void;
}) {
	const navigate = useNavigate();
	return (
		<div className="flex flex-col gap-4">
			{isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription className="flex items-center justify-between gap-4">
						{text.loadFailed}
						<Button type="button" variant="outline" size="sm" onClick={onRetry}>
							{copy.errors.retry}
						</Button>
					</AlertDescription>
				</Alert>
			)}
			{channels?.length === 0 && (
				<Empty className="py-16">
					<EmptyHeader>
						<EmptyTitle>{text.emptyHeading}</EmptyTitle>
						<EmptyDescription>{text.emptyBody}</EmptyDescription>
					</EmptyHeader>
					<EmptyContent>
						<Button variant="outline" onClick={onAdd}>
							{copy.notifications.add}
						</Button>
					</EmptyContent>
				</Empty>
			)}
			{(channels === undefined ? !isError : channels.length > 0) && (
				<div className="@container overflow-hidden rounded-lg border bg-card">
					<Table>
						{channels && (
							<TableCaption className="sr-only">
								{text.channelsAria(channels.length)}
							</TableCaption>
						)}
						<TableHeader className="bg-muted">
							<TableRow>
								<TableHead className="px-4">{text.columns.name}</TableHead>
								<TableHead className={cn("px-4", SHOW_TYPE)}>
									{text.columns.type}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_ALERTS)}>
									{text.columns.alertTypes}
								</TableHead>
								<TableHead className="px-4">
									{text.columns.lastDelivery}
								</TableHead>
								<TableHead className="w-16 px-4">
									<span className="sr-only">{text.actionsColumn}</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{channels === undefined ? (
								<SkeletonRows />
							) : (
								channels.map((channel) => (
									<TableRow
										key={channel.id}
										data-channel-row={channel.id}
										className={cn(
											"h-16 cursor-pointer",
											!channel.enabled && "text-muted-foreground",
										)}
										onClick={() =>
											void navigate({
												to: "/settings/notifications/$id",
												params: { id: channel.id },
											})
										}
									>
										<TableCell className="px-4">
											<div className="flex flex-wrap items-center gap-2">
												<Link
													to="/settings/notifications/$id"
													params={{ id: channel.id }}
													title={channel.name}
													onClick={(event) => event.stopPropagation()}
													className="max-w-40 truncate rounded-sm text-base font-semibold hover:underline @min-[840px]:max-w-56"
												>
													{channel.name}
												</Link>
												{channel.source === "config" && (
													<Badge variant="outline" className="h-6 text-sm">
														<Lock aria-hidden />
														{text.fromConfig}
													</Badge>
												)}
											</div>
										</TableCell>
										<TableCell className={cn("px-4", SHOW_TYPE)}>
											<KindLabel kind={channel.type} />
										</TableCell>
										<TableCell className={cn("px-4", SHOW_ALERTS)}>
											<AlertTypes channel={channel} />
										</TableCell>
										<TableCell className="px-4">
											<LastDelivery channel={channel} />
										</TableCell>
										<TableCell
											className="px-4"
											onClick={(event) => event.stopPropagation()}
										>
											<RowActions channel={channel} onDelete={onDelete} />
										</TableCell>
									</TableRow>
								))
							)}
						</TableBody>
					</Table>
				</div>
			)}
		</div>
	);
}
