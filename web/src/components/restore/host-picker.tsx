import { ChevronDown, ChevronsUpDown, Server } from "lucide-react";
import { memo, useState } from "react";
import { useListRestoreHosts } from "@/api/gen/endpoints";
import type { RestoreHost } from "@/api/gen/model";
import { UnverifiedBadge } from "@/components/snapshots/suspicious-badge";
import { StatusBadge } from "@/components/status-badge";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
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
import { hostCount } from "@/lib/plural";

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
	return (
		<>
			{host.unverified_count > 0 && (
				<UnverifiedBadge plain count={host.unverified_count} />
			)}
			{host.latest_snapshot_ms == null && host.unverified_count > 0 && (
				<span className="min-w-0 truncate text-sm font-normal text-muted-foreground">
					{copy.restore.hostPicker.noSigned}
				</span>
			)}
			{host.agent && host.status ? (
				<StatusBadge status={host.status} />
			) : (
				<Badge variant="outline">{copy.restore.hostPicker.noAgentBadge}</Badge>
			)}
		</>
	);
}

function HostItem({
	host,
	source,
	checked,
	onSelect,
}: {
	host: RestoreHost;
	source?: boolean;
	checked?: boolean;
	onSelect: (key: string) => void;
}) {
	return (
		<CommandItem
			value={host.key}
			onSelect={() => onSelect(host.key)}
			aria-checked={checked}
			className="h-10 gap-2"
		>
			{checked !== undefined && (
				<Checkbox checked={checked} aria-hidden tabIndex={-1} />
			)}
			<bdi className="max-w-3/5 shrink-0 truncate font-semibold">
				{displayName(host.hostname)}
			</bdi>
			{labelText(host)
				.slice(0, MAX_LABELS)
				.map((label) => (
					<code
						key={label}
						className="shrink-100 truncate text-sm font-normal text-muted-foreground"
					>
						<bdi>{displayName(label)}</bdi>
					</code>
				))}
			<span className="ml-auto flex min-w-0 items-center gap-2">
				{source && (
					<span className="min-w-0 truncate text-sm text-muted-foreground">
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

const selectionSummary = (
	hosts: RestoreHost[],
	selected: string[],
	labels: string[],
) => {
	const names = selected.map(
		(key) => hosts.find((host) => host.key === key)?.hostname,
	);
	const parts = [
		selected.length === 1 && names[0] !== undefined
			? displayName(names[0])
			: selected.length > 1
				? hostCount(selected.length)
				: null,
		...labels,
	].filter((part) => part !== null);
	return parts.length > 0 ? parts.join(", ") : copy.find.filters.allHosts;
};

export const HostPicker = memo(function HostPicker({
	value,
	onChange,
	mode = "source",
	sourceKey,
	invalid,
	describedBy,
	selected,
	selectedLabels = [],
	onToggleHost,
	onToggleLabel,
}: {
	value?: string;
	onChange?: (key: string) => void;
	mode?: "source" | "target";
	sourceKey?: string;
	invalid?: boolean;
	describedBy?: string;
	selected?: string[];
	selectedLabels?: string[];
	onToggleHost?: (key: string) => void;
	onToggleLabel?: (label: string) => void;
}) {
	const multi = onToggleHost !== undefined;
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

	const labelOptions = multi
		? [...new Set(all.flatMap(labelText))]
				.filter((label) => normalize(label).includes(normalize(query)))
				.sort()
				.slice(0, LIMIT)
		: [];

	function choose(key: string) {
		if (multi) return onToggleHost(key);
		setOpen(false);
		onChange?.(key);
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
				{multi ? (
					<Button type="button" variant="outline" size="sm">
						<Server aria-hidden />
						<bdi className="max-w-64 truncate">
							{selectionSummary(all, selected ?? [], selectedLabels)}
						</bdi>
						<ChevronDown aria-hidden />
					</Button>
				) : (
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
						className="h-auto min-h-10 w-full justify-between py-2"
					>
						{current ? (
							<>
								<bdi className="max-w-2/5 shrink-0 truncate font-semibold">
									{displayName(current.hostname)}
								</bdi>
								<span className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-x-2 gap-y-1">
									{current.key === sourceKey && (
										<span className="min-w-0 truncate text-sm font-normal text-muted-foreground">
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
						<ChevronsUpDown
							aria-hidden
							className="size-4 shrink-0 opacity-50"
						/>
					</Button>
				)}
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
										checked={multi ? selected?.includes(host.key) : undefined}
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
									<HostItem
										key={host.key}
										host={host}
										checked={multi ? selected?.includes(host.key) : undefined}
										onSelect={choose}
									/>
								))}
							</CommandGroup>
						)}
						{labelOptions.length > 0 && (
							<CommandGroup heading={copy.hostPage.cards.labels}>
								{labelOptions.map((label) => (
									<CommandItem
										key={label}
										value={`label:${label}`}
										onSelect={() => onToggleLabel?.(label)}
										aria-checked={selectedLabels.includes(label)}
										className="h-10 gap-2"
									>
										<Checkbox
											checked={selectedLabels.includes(label)}
											aria-hidden
											tabIndex={-1}
										/>
										<code className="truncate text-sm">
											<bdi>{displayName(label)}</bdi>
										</code>
									</CommandItem>
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
