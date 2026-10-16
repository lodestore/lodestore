import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Copy } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";
import { toast } from "sonner";
import type { HostDetail, HostMount } from "@/api/gen/model";
import { HostPolicies } from "@/components/hosts/host-policies";
import { CertificateText, VersionCell } from "@/components/hosts/hosts-table";
import { PathText } from "@/components/path-text";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
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
import { formatBytes } from "@/lib/format";
import { infoQuery } from "@/lib/session";

const FILESYSTEMS_SHOWN = 20;
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

const present = (value: Fact[1]) =>
	value !== null && value !== undefined && value !== "";

function InfoCard({
	title,
	className,
	children,
}: {
	title: string;
	className?: string;
	children: ReactNode;
}) {
	return (
		<Card className={className}>
			<CardHeader>
				<CardTitle>
					<h2>{title}</h2>
				</CardTitle>
			</CardHeader>
			<CardContent className="flex flex-col gap-2">{children}</CardContent>
		</Card>
	);
}

function FactsCard({ title, facts }: { title: string; facts: Fact[] }) {
	if (!facts.some(([, value]) => present(value))) return null;
	return (
		<InfoCard title={title}>
			<dl className="grid auto-rows-[minmax(1.5rem,auto)] grid-cols-[8rem_1fr] items-center gap-x-4 gap-y-2 text-sm">
				{facts.map(([label, value]) => (
					<div key={label} className="contents">
						<dt className="text-muted-foreground">{label}</dt>
						<dd className="min-w-0 break-words">
							{present(value) ? value : copy.common.dash}
						</dd>
					</div>
				))}
			</dl>
		</InfoCard>
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
		<InfoCard title={d.storage} className="lg:col-span-2">
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
					{shown.map((m, index) => (
						<TableRow key={`${m.path}:${m.fs_type}:${index}`}>
							<TableCell className="font-mono whitespace-normal">
								<PathText value={m.path} />
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
					variant="outline"
					size="sm"
					className="self-start"
					onClick={() => setAll(true)}
				>
					{copy.hosts.showAllFilesystems(sorted.length)}
				</Button>
			)}
		</InfoCard>
	);
}

function AgentId({ id }: { id: string }) {
	const idRef = useRef<HTMLSpanElement>(null);
	async function onCopy() {
		if (await copyText(id, idRef.current)) toast.success(copy.common.copied);
	}
	return (
		<span className="inline-flex items-center gap-2">
			<span ref={idRef} className="font-mono">
				{id}
			</span>
			<Button
				variant="ghost"
				size="icon-xs"
				className="relative -my-1 shrink-0 after:absolute after:-inset-[9px]"
				aria-label={copy.hosts.details.copyAgentId}
				onClick={onCopy}
			>
				<Copy aria-hidden />
			</Button>
		</span>
	);
}

function LabelsCard({ host }: { host: HostDetail }) {
	if (host.labels.length === 0) return null;
	return (
		<InfoCard title={copy.hostPage.cards.labels}>
			<div className="flex flex-wrap gap-2">
				{host.labels.map((label) => (
					<Badge
						key={label.key}
						variant="outline"
						className="h-auto max-w-full font-mono wrap-anywhere whitespace-normal"
					>
						{label.key}={label.value}
					</Badge>
				))}
			</div>
		</InfoCard>
	);
}

export function HostOverview({ host }: { host: HostDetail }) {
	const d = copy.hosts.details;
	const info = useQuery(infoQuery);
	return (
		<div className="grid gap-6 xl:grid-cols-2">
			<FactsCard
				title={copy.hostPage.cards.status}
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
					[
						d.repository,
						<Link
							key="r"
							to="/settings/repositories/$id"
							params={{ id: host.repository_id }}
							className="rounded-sm text-primary-strong underline-offset-4 hover:underline"
						>
							{host.repository_name}
						</Link>,
					],
				]}
			/>
			<FactsCard
				title={copy.hostPage.cards.system}
				facts={[
					[d.os, host.os],
					[d.arch, host.arch],
					[d.distro, host.distro],
					[d.kernel, host.kernel],
					[
						d.uptime,
						host.uptime_s == null || host.uptime_s <= 0
							? null
							: formatUptime(host.uptime_s),
					],
					[
						copy.hosts.columns.version,
						<VersionCell
							key="v"
							version={host.version}
							serverVersion={info.data?.version}
						/>,
					],
				]}
			/>
			<LabelsCard host={host} />
			<HostPolicies host={host} />
			<FactsCard
				title={copy.hostPage.cards.certificate}
				facts={[
					[d.notBefore, formatDate(host.cert_issued_at)],
					[
						d.notAfter,
						<span key="a" className="inline-flex flex-wrap gap-x-2">
							{formatDate(host.cert_not_after)}
							<CertificateText host={host} />
						</span>,
					],
					[d.nextRenewal, formatDate(host.next_renewal_at)],
				]}
			/>
			<Filesystems mounts={host.mounts} />
		</div>
	);
}

export function HostOverviewSkeleton() {
	return (
		<div className="grid gap-6 xl:grid-cols-2" aria-hidden>
			{[5, 6, 3, 3].map((rows, i) => (
				<Card key={i}>
					<CardHeader>
						<Skeleton className="h-4 w-24" />
					</CardHeader>
					<CardContent className="flex flex-col gap-2">
						{Array.from({ length: rows }, (_, j) => (
							<Skeleton key={j} className="h-4 w-full" />
						))}
					</CardContent>
				</Card>
			))}
		</div>
	);
}
