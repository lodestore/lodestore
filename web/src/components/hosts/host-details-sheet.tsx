import { Copy } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";
import { toast } from "sonner";
import { useGetHost } from "@/api/gen/endpoints";
import type { Host, HostDetail, HostMount } from "@/api/gen/model";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
	Sheet,
	SheetContent,
	SheetDescription,
	SheetFooter,
	SheetHeader,
	SheetTitle,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import {
	Table,
	TableBody,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";

const FILESYSTEMS_SHOWN = 20;
const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

function formatBytes(bytes: number) {
	let value = bytes;
	let unit = 0;
	while (value >= 1024 && unit < UNITS.length - 1) {
		value /= 1024;
		unit += 1;
	}
	return `${unit === 0 ? value : value.toFixed(1)} ${UNITS[unit]}`;
}

function formatUptime(seconds: number) {
	const days = Math.floor(seconds / 86_400);
	const hours = Math.floor((seconds % 86_400) / 3_600);
	const minutes = Math.floor((seconds % 3_600) / 60);
	if (days > 0) return `${days}d ${hours}h`;
	if (hours > 0) return `${hours}h ${minutes}m`;
	return `${minutes}m`;
}

function formatDate(ms: number) {
	return (
		<time dateTime={new Date(ms).toISOString()}>
			{new Date(ms).toLocaleString("en", {
				dateStyle: "medium",
				timeStyle: "short",
			})}
		</time>
	);
}

const usedPercent = (m: HostMount) =>
	m.total_bytes > 0
		? Math.round(((m.total_bytes - m.free_bytes) / m.total_bytes) * 100)
		: 0;

type Fact = [label: string, value: ReactNode | null | undefined];

function Section({ title, facts }: { title: string; facts: Fact[] }) {
	const present = (value: Fact[1]) =>
		value !== null && value !== undefined && value !== "";
	if (!facts.some(([, value]) => present(value))) return null;
	return (
		<section className="flex flex-col gap-2">
			<h3 className="text-sm font-semibold">{title}</h3>
			<dl className="grid grid-cols-[8rem_1fr] gap-x-4 gap-y-1.5 text-sm">
				{facts.map(([label, value]) => (
					<div key={label} className="contents">
						<dt className="text-muted-foreground">{label}</dt>
						<dd className="min-w-0 break-all">
							{present(value) ? value : copy.common.dash}
						</dd>
					</div>
				))}
			</dl>
		</section>
	);
}

function Filesystems({ mounts }: { mounts: HostMount[] }) {
	const [all, setAll] = useState(false);
	if (mounts.length === 0) return null;
	const sorted = [...mounts].sort(
		(a, b) => usedPercent(b) - usedPercent(a) || a.path.localeCompare(b.path),
	);
	const shown = all ? sorted : sorted.slice(0, FILESYSTEMS_SHOWN);
	const d = copy.hosts.details;
	return (
		<section className="flex flex-col gap-2">
			<h3 className="text-sm font-semibold">{d.storage}</h3>
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>{d.path}</TableHead>
						<TableHead>{d.type}</TableHead>
						<TableHead className="text-right">{d.free}</TableHead>
						<TableHead className="text-right">{d.used}</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{shown.map((m) => (
						<TableRow key={m.path}>
							<TableCell className="font-mono break-all whitespace-normal">
								{m.path}
							</TableCell>
							<TableCell>{m.fs_type}</TableCell>
							<TableCell className="text-right tabular-nums">
								{formatBytes(m.free_bytes)}
							</TableCell>
							<TableCell className="text-right tabular-nums">
								{usedPercent(m)}%
							</TableCell>
						</TableRow>
					))}
				</TableBody>
			</Table>
			{!all && sorted.length > FILESYSTEMS_SHOWN && (
				<Button
					variant="ghost"
					size="sm"
					className="self-start"
					onClick={() => setAll(true)}
				>
					{copy.hosts.showAllFilesystems(sorted.length)}
				</Button>
			)}
		</section>
	);
}

function AgentId({ id }: { id: string }) {
	const idRef = useRef<HTMLSpanElement>(null);
	async function onCopy() {
		if (await copyText(id, idRef.current)) toast.success(copy.common.copied);
	}
	return (
		<span className="flex items-start gap-1">
			<span ref={idRef} className="font-mono">
				{id}
			</span>
			<Button
				variant="ghost"
				size="icon-sm"
				className="shrink-0"
				aria-label={copy.hosts.details.copyAgentId}
				onClick={onCopy}
			>
				<Copy aria-hidden />
			</Button>
		</span>
	);
}

function Details({ host }: { host: HostDetail }) {
	const d = copy.hosts.details;
	return (
		<div className="flex flex-col gap-6">
			<Section
				title={d.overview}
				facts={[
					[d.status, <StatusBadge key="s" status={host.status} />],
					[
						d.lastContact,
						host.last_seen_at == null ? null : (
							<RelativeTime value={new Date(host.last_seen_at)} />
						),
					],
					[d.enrolled, formatDate(host.enrolled_at)],
					[d.agentId, <AgentId key="id" id={host.id} />],
					[d.repository, host.repository_name],
				]}
			/>
			<Section
				title={d.system}
				facts={[
					[d.os, host.os],
					[d.arch, host.arch],
					[d.distro, host.distro],
					[d.kernel, host.kernel],
					[
						d.uptime,
						host.uptime_s == null ? null : formatUptime(host.uptime_s),
					],
				]}
			/>
			<Filesystems mounts={host.mounts} />
			<Section
				title={d.certificate}
				facts={[
					[d.notBefore, formatDate(host.cert_issued_at)],
					[d.notAfter, formatDate(host.cert_not_after)],
					[d.nextRenewal, formatDate(host.next_renewal_at)],
				]}
			/>
		</div>
	);
}

function DetailsSkeleton() {
	return (
		<div className="flex flex-col gap-6" aria-hidden>
			{[5, 5, 4, 3].map((rows, i) => (
				<div key={i} className="flex flex-col gap-2">
					<Skeleton className="h-4 w-24" />
					{Array.from({ length: rows }, (_, j) => (
						<Skeleton key={j} className="h-4 w-full" />
					))}
				</div>
			))}
		</div>
	);
}

export function HostDetailsSheet({
	host,
	onClose,
	onEditLabels,
	onRevoke,
	onCloseAutoFocus,
}: {
	host?: Host;
	onClose: () => void;
	onEditLabels: (host: Host) => void;
	onRevoke: (host: Host) => void;
	onCloseAutoFocus?: (event: Event) => void;
}) {
	const details = useGetHost(host?.id ?? "", {
		query: { enabled: host !== undefined },
	});
	const loaded = details.data;

	return (
		<Sheet
			open={host !== undefined}
			onOpenChange={(open) => {
				if (!open) onClose();
			}}
		>
			<SheetContent
				className="data-[side=right]:w-full data-[side=right]:sm:max-w-[480px]"
				onCloseAutoFocus={onCloseAutoFocus}
			>
				<SheetHeader>
					<SheetTitle className="break-all">{host?.hostname}</SheetTitle>
					<SheetDescription className="sr-only">
						{copy.hosts.details.description}
					</SheetDescription>
				</SheetHeader>
				<ScrollArea className="min-h-0 flex-1">
					<div className="px-4 pb-4">
						{details.isError ? (
							<Alert variant="destructive" role="alert">
								<AlertDescription>
									{copy.errors.hostDetails}{" "}
									<Button
										variant="link"
										className="h-auto p-0"
										onClick={() => details.refetch()}
									>
										{copy.errors.retry}
									</Button>
								</AlertDescription>
							</Alert>
						) : loaded ? (
							<Details host={loaded} />
						) : (
							<DetailsSkeleton />
						)}
					</div>
				</ScrollArea>
				{loaded && loaded.status !== "revoked" && (
					<SheetFooter className="flex-row justify-end">
						<Button variant="outline" onClick={() => onEditLabels(loaded)}>
							{copy.hosts.editLabels}
						</Button>
						<Button variant="destructive" onClick={() => onRevoke(loaded)}>
							{copy.hosts.revokeHost}
						</Button>
					</SheetFooter>
				)}
			</SheetContent>
		</Sheet>
	);
}
