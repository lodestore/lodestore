import { useQuery } from "@tanstack/react-query";
import { type NavigateOptions, useNavigate } from "@tanstack/react-router";
import {
	Bell,
	BellPlus,
	Eraser,
	KeyRound,
	LogOut,
	Monitor,
	Moon,
	Plus,
	Search,
	Server,
	ShieldCheck,
	Sun,
	UserRound,
} from "lucide-react";
import { type ReactNode, useLayoutEffect, useRef, useState } from "react";
import { getListHostsQueryOptions } from "@/api/gen/endpoints";
import type { Host } from "@/api/gen/model";
import { NAV_ITEMS } from "@/components/app-sidebar";
import { RECLAIM_ANCHOR } from "@/components/repos/reclaim/reclaim-card";
import { StatusBadge } from "@/components/status-badge";
import { useTheme } from "@/components/theme/theme-provider";
import {
	Command,
	CommandDialog,
	CommandEmpty,
	CommandGroup,
	CommandInput,
	CommandItem,
	CommandList,
} from "@/components/ui/command";
import { Kbd } from "@/components/ui/kbd";
import { Skeleton } from "@/components/ui/skeleton";
import { copy } from "@/copy";
import { repositoriesQuery } from "@/lib/session";
import { useSignOut } from "@/lib/sign-out";

const EXTRA_PAGES: typeof NAV_ITEMS = [
	{
		label: copy.notifications.title,
		to: "/settings/notifications",
		icon: Bell,
		group: copy.shell.groups.system,
	},
	{
		label: copy.tokens.title,
		to: "/settings/tokens",
		icon: KeyRound,
		group: copy.shell.groups.system,
	},
	{
		label: copy.user.title,
		to: "/settings/user",
		icon: UserRound,
		group: copy.shell.groups.system,
	},
];

const normalize = (value: string) => value.trim().toLocaleLowerCase();
const looksLikeFile = (value: string) =>
	/^\S+$/.test(value) && /[.*/]/.test(value);

const STATUS_RANK: Record<Host["status"], number> = {
	online: 0,
	offline: 1,
	revoked: 2,
};

const labelText = (host: Host) =>
	host.labels.map(({ key, value }) => `${key}=${value}`);

function matchHosts(hosts: Host[], query: string) {
	return hosts
		.filter((host) => {
			if (host.status === "revoked" && query === "") return false;
			return (
				normalize(host.hostname).includes(query) ||
				labelText(host).some((label) => normalize(label).includes(query))
			);
		})
		.sort(
			(a, b) =>
				STATUS_RANK[a.status] - STATUS_RANK[b.status] ||
				a.hostname.localeCompare(b.hostname),
		)
		.slice(0, 8);
}

export function CommandPalette({
	open,
	onOpenChange,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
}) {
	const signOut = useSignOut();
	const opener = useRef<HTMLElement | null>(null);
	const navigated = useRef(false);
	useLayoutEffect(() => {
		if (!open) return;
		opener.current = document.activeElement as HTMLElement | null;
		navigated.current = false;
	}, [open]);
	return (
		<CommandDialog
			open={open}
			onCloseAutoFocus={(event) => {
				event.preventDefault();
				if (!navigated.current) opener.current?.focus();
			}}
			onOpenChange={onOpenChange}
			title={copy.palette.label}
			description={copy.palette.placeholder}
			className="sm:max-w-[560px]"
		>
			<PaletteBody
				close={() => onOpenChange(false)}
				onNavigate={() => {
					navigated.current = true;
				}}
				signOut={signOut}
			/>
		</CommandDialog>
	);
}

function Hint({ text }: { text: string }) {
	const [key, ...rest] = text.split(" ");
	return (
		<span className="flex items-center gap-2">
			<Kbd>{key}</Kbd>
			{rest.join(" ")}
		</span>
	);
}

function LoadingRows() {
	return (
		<div className="flex flex-col gap-2 px-2 py-2">
			<Skeleton className="h-5 w-full" />
			<Skeleton className="h-5 w-full" />
			<Skeleton className="h-5 w-full" />
		</div>
	);
}

function Group({
	heading,
	pending,
	failed,
	children,
	empty,
}: {
	heading: string;
	pending: boolean;
	failed: string | false;
	children: ReactNode;
	empty: boolean;
}) {
	if (!pending && !failed && empty) return null;
	return (
		<CommandGroup heading={heading}>
			{pending ? (
				<LoadingRows />
			) : failed ? (
				<CommandItem disabled value={failed}>
					{failed}
				</CommandItem>
			) : (
				children
			)}
		</CommandGroup>
	);
}

function PaletteBody({
	close,
	onNavigate,
	signOut,
}: {
	close: () => void;
	onNavigate: () => void;
	signOut: () => void;
}) {
	const navigate = useNavigate();
	const { setTheme } = useTheme();
	const [search, setSearch] = useState("");
	const query = normalize(search);
	const hostsResult = useQuery(getListHostsQueryOptions());
	const reposResult = useQuery(repositoriesQuery);

	const run = (action: () => void) => () => {
		close();
		action();
	};
	const go = (options: NavigateOptions) =>
		run(() => {
			onNavigate();
			void navigate(options);
		});

	const pages = [...NAV_ITEMS, ...EXTRA_PAGES].filter(({ label, pageLabel }) =>
		[label, pageLabel].some((name) => normalize(name ?? "").includes(query)),
	);
	const hosts = matchHosts(hostsResult.data ?? [], query);
	const repositories = (reposResult.data ?? [])
		.filter(
			(repo) =>
				normalize(repo.name).includes(query) ||
				normalize(repo.location_display).includes(query),
		)
		.slice(0, 8);

	const noRepositories = reposResult.data?.length === 0;
	const allRepositories = reposResult.data ?? [];
	const cleanup = allRepositories.slice(0, 8).map((repo) => ({
		label:
			allRepositories.length === 1
				? copy.palette.freeUpSpace
				: copy.palette.freeUpSpaceIn(repo.name),
		icon: Eraser,
		keywords: copy.palette.cleanupKeywords,
		run: go({
			to: "/settings/repositories/$id",
			params: { id: repo.id },
			hash: RECLAIM_ANCHOR,
		}),
	}));
	const allActions: {
		label: string;
		icon: typeof Eraser;
		keywords?: string;
		always?: boolean;
		run: () => void;
	}[] = [
		{
			label: copy.palette.findFile,
			icon: Search,
			always: looksLikeFile(search.trim()),
			run: go({
				to: "/restore",
				search: {
					tab: "find",
					find: looksLikeFile(search.trim()) ? search.trim() : undefined,
				},
			}),
		},
		{
			label: copy.palette.addHost,
			icon: Server,
			run: noRepositories
				? go({ to: "/welcome" })
				: go({ to: "/hosts", search: { add: true } }),
		},
		{
			label: copy.palette.addRepository,
			icon: Plus,
			run: go({ to: "/settings/repositories/new" }),
		},
		{
			label: copy.palette.createPolicy,
			icon: ShieldCheck,
			run: go({ to: "/policies/new" }),
		},
		{
			label: copy.palette.createApiToken,
			icon: KeyRound,
			run: go({ to: "/settings/tokens", search: { create: true } }),
		},
		{
			label: copy.notifications.paletteAdd,
			icon: BellPlus,
			run: go({ to: "/settings/notifications/new" }),
		},
		{
			label: copy.palette.lightTheme,
			icon: Sun,
			run: run(() => setTheme("light")),
		},
		{
			label: copy.palette.darkTheme,
			icon: Moon,
			run: run(() => setTheme("dark")),
		},
		{
			label: copy.palette.systemTheme,
			icon: Monitor,
			run: run(() => setTheme("system")),
		},
		...cleanup,
		{ label: copy.signOut, icon: LogOut, run: run(signOut) },
	];
	const actions = allActions.filter(
		({ label, keywords, always }) =>
			always ||
			normalize(label).includes(query) ||
			(query !== "" &&
				(keywords ?? "").split(" ").some((word) => word.startsWith(query))),
	);

	const pending = hostsResult.isPending || reposResult.isPending;

	return (
		<Command shouldFilter={false} label={copy.palette.label}>
			<CommandInput
				value={search}
				onValueChange={setSearch}
				placeholder={copy.palette.placeholder}
			/>
			<CommandList className="max-h-[400px]">
				{!pending && (
					<CommandEmpty>{copy.palette.empty(search.trim())}</CommandEmpty>
				)}
				{pages.length > 0 && (
					<CommandGroup heading={copy.palette.groups.pages}>
						{pages.map(({ label, pageLabel, to, icon: Icon, soon }) => (
							<CommandItem key={to} value={to} onSelect={go({ to })}>
								<Icon />
								<span className="truncate">{pageLabel ?? label}</span>
								{soon && (
									<span className="ml-auto text-xs text-muted-foreground">
										{copy.soon}
									</span>
								)}
							</CommandItem>
						))}
					</CommandGroup>
				)}
				<Group
					heading={copy.palette.groups.hosts}
					pending={hostsResult.isPending}
					failed={hostsResult.isError && copy.palette.hostsFailed}
					empty={hosts.length === 0}
				>
					{hosts.map((host) => (
						<CommandItem
							key={host.id}
							value={`host-${host.id}`}
							onSelect={go({
								to: "/hosts/$id",
								params: { id: host.id },
							})}
						>
							<span className="min-w-0 flex-1 truncate">{host.hostname}</span>
							<StatusBadge status={host.status} className="shrink-0" />
							{host.labels.length > 0 && (
								<span className="max-w-[40%] truncate font-mono text-xs text-muted-foreground">
									{labelText(host).slice(0, 2).join(" ")}
								</span>
							)}
						</CommandItem>
					))}
				</Group>
				<Group
					heading={copy.palette.groups.repositories}
					pending={reposResult.isPending}
					failed={reposResult.isError && copy.palette.repositoriesFailed}
					empty={repositories.length === 0}
				>
					{repositories.map((repo) => (
						<CommandItem
							key={repo.id}
							value={`repository-${repo.id}`}
							onSelect={go({
								to: "/settings/repositories/$id",
								params: { id: repo.id },
							})}
						>
							<span className="min-w-0 flex-1 truncate">{repo.name}</span>
							<span className="max-w-[50%] truncate font-mono text-xs text-muted-foreground">
								{repo.location_display}
							</span>
						</CommandItem>
					))}
				</Group>
				{actions.length > 0 && (
					<CommandGroup heading={copy.palette.groups.actions}>
						{actions.map(({ label, icon: Icon, run: onSelect }) => (
							<CommandItem key={label} value={label} onSelect={onSelect}>
								<Icon />
								{label}
							</CommandItem>
						))}
					</CommandGroup>
				)}
			</CommandList>
			<div className="flex gap-4 border-t px-4 py-2 text-xs text-muted-foreground">
				<Hint text={copy.palette.hints.move} />
				<Hint text={copy.palette.hints.open} />
				<Hint text={copy.palette.hints.close} />
			</div>
		</Command>
	);
}
