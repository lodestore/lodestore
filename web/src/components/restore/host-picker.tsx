import { ChevronsUpDown } from "lucide-react";
import { memo, useState } from "react";
import { useListRestoreHosts } from "@/api/gen/endpoints";
import type { RestoreHost } from "@/api/gen/model";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Command,
	CommandEmpty,
	CommandGroup,
	CommandInput,
	CommandItem,
	CommandList,
} from "@/components/ui/command";
import {
	Popover,
	PopoverContent,
	PopoverTrigger,
} from "@/components/ui/popover";
import { copy } from "@/copy";
import { displayName } from "@/lib/names";

const LIMIT = 50;
const MAX_LABELS = 2;
const RANK = { online: 0, offline: 1, revoked: 2 } as const;

const normalize = (value: string) => value.trim().toLocaleLowerCase();
const labelText = (host: RestoreHost) =>
	host.labels.map(({ key, value }) => `${key}=${value}`);

function matches(hosts: RestoreHost[], query: string) {
	const wanted = normalize(query);
	return hosts
		.filter(
			(host) =>
				normalize(host.hostname).includes(wanted) ||
				labelText(host).some((label) => normalize(label).includes(wanted)),
		)
		.sort(
			(a, b) =>
				RANK[a.status ?? "online"] - RANK[b.status ?? "online"] ||
				a.hostname.localeCompare(b.hostname),
		);
}

function Status({ host }: { host: RestoreHost }) {
	return host.agent && host.status ? (
		<StatusBadge status={host.status} />
	) : (
		<Badge variant="outline">{copy.restore.hostPicker.noAgentBadge}</Badge>
	);
}

function HostItem({
	host,
	source,
	onSelect,
}: {
	host: RestoreHost;
	source?: boolean;
	onSelect: (key: string) => void;
}) {
	return (
		<CommandItem
			value={host.key}
			onSelect={() => onSelect(host.key)}
			className="h-10 gap-2"
		>
			<bdi className="truncate font-semibold">{displayName(host.hostname)}</bdi>
			{labelText(host)
				.slice(0, MAX_LABELS)
				.map((label) => (
					<code
						key={label}
						className="truncate text-sm font-normal text-muted-foreground"
					>
						<bdi>{displayName(label)}</bdi>
					</code>
				))}
			<span className="ml-auto flex shrink-0 items-center gap-2">
				{source && (
					<span className="text-sm text-muted-foreground">
						{copy.restore.hostPicker.sourceHost}
					</span>
				)}
				<Status host={host} />
			</span>
		</CommandItem>
	);
}

export const isRestoreTarget = (host: RestoreHost) =>
	host.agent && host.host_id != null && host.status !== "revoked";

export const HostPicker = memo(function HostPicker({
	value,
	onChange,
	mode = "source",
	sourceKey,
	invalid,
	describedBy,
}: {
	value?: string;
	onChange: (key: string) => void;
	mode?: "source" | "target";
	sourceKey?: string;
	invalid?: boolean;
	describedBy?: string;
}) {
	const hosts = useListRestoreHosts();
	const [open, setOpen] = useState(false);
	const [query, setQuery] = useState("");

	const all = hosts.data ?? [];
	const current = all.find((host) => host.key === value);
	const found = matches(
		mode === "target" ? all.filter(isRestoreTarget) : all,
		query,
	);
	const shown = found.slice(0, LIMIT);
	const withAgent = shown.filter((host) => host.agent);
	const withoutAgent = shown.filter((host) => !host.agent);

	function choose(key: string) {
		setOpen(false);
		onChange(key);
	}

	return (
		<Popover
			open={open}
			onOpenChange={(next) => {
				setOpen(next);
				if (!next) setQuery("");
			}}
		>
			<PopoverTrigger asChild>
				<Button
					type="button"
					variant="outline"
					role="combobox"
					aria-invalid={invalid}
					aria-describedby={describedBy}
					aria-label={
						current
							? copy.restore.hostPicker.current(displayName(current.hostname))
							: copy.restore.hostPicker.label
					}
					className="w-full justify-between"
				>
					{current ? (
						<>
							<bdi className="truncate font-semibold">
								{displayName(current.hostname)}
							</bdi>
							<span className="ml-auto flex shrink-0 items-center gap-2">
								{current.key === sourceKey && (
									<span className="text-sm font-normal text-muted-foreground">
										{copy.restore.hostPicker.sourceHost}
									</span>
								)}
								<Status host={current} />
							</span>
						</>
					) : (
						<span className="text-muted-foreground">
							{copy.restore.hostPicker.choose}
						</span>
					)}
					<ChevronsUpDown aria-hidden className="size-4 shrink-0 opacity-50" />
				</Button>
			</PopoverTrigger>
			<PopoverContent
				align="start"
				className="w-(--radix-popover-trigger-width) min-w-90 gap-0 p-0"
			>
				<Command shouldFilter={false}>
					<CommandInput
						value={query}
						onValueChange={setQuery}
						placeholder={copy.restore.hostPicker.search}
						aria-label={copy.restore.hostPicker.search}
					/>
					<CommandList>
						{query.trim() && (
							<CommandEmpty>
								{copy.restore.hostPicker.noMatch(query.trim())}
							</CommandEmpty>
						)}
						{withAgent.length > 0 && (
							<CommandGroup heading={copy.restore.hostPicker.hosts}>
								{withAgent.map((host) => (
									<HostItem
										key={host.key}
										host={host}
										source={host.key === sourceKey}
										onSelect={choose}
									/>
								))}
							</CommandGroup>
						)}
						{withoutAgent.length > 0 && (
							<CommandGroup heading={copy.restore.hostPicker.noAgentGroup}>
								<p className="px-2 pb-1 text-sm text-muted-foreground">
									{copy.restore.hostPicker.noAgentGroupHint}
								</p>
								{withoutAgent.map((host) => (
									<HostItem key={host.key} host={host} onSelect={choose} />
								))}
							</CommandGroup>
						)}
						{found.length > LIMIT && (
							<p className="p-2 text-sm text-muted-foreground">
								{copy.restore.hostPicker.keepTyping}
							</p>
						)}
					</CommandList>
				</Command>
			</PopoverContent>
		</Popover>
	);
});
