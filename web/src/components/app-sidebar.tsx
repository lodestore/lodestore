import { useQuery } from "@tanstack/react-query";
import { Link, type LinkProps, useRouterState } from "@tanstack/react-router";
import {
	ArchiveRestore,
	ChevronsUpDown,
	ExternalLink,
	LayoutDashboard,
	ListChecks,
	type LucideIcon,
	Monitor,
	Moon,
	Server,
	Settings,
	ShieldCheck,
	Sun,
} from "lucide-react";
import { useCountAlerts } from "@/api/gen/endpoints";
import { LogoMark } from "@/components/brand/logo-mark";
import { type Theme, useTheme } from "@/components/theme/theme-provider";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuLabel,
	DropdownMenuRadioGroup,
	DropdownMenuRadioItem,
	DropdownMenuSeparator,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ModK } from "@/components/ui/kbd";
import {
	Sidebar,
	SidebarContent,
	SidebarFooter,
	SidebarGroup,
	SidebarGroupContent,
	SidebarGroupLabel,
	SidebarHeader,
	SidebarMenu,
	SidebarMenuBadge,
	SidebarMenuButton,
	SidebarMenuItem,
	useSidebar,
} from "@/components/ui/sidebar";
import { copy } from "@/copy";
import { infoQuery, sessionQuery } from "@/lib/session";
import { useSignOut } from "@/lib/sign-out";

export type NavItem = {
	label: string;
	pageLabel?: string;
	to: NonNullable<LinkProps["to"]>;
	icon: LucideIcon;
	soon?: boolean;
	group: string;
};

const { workspace, backup, system } = copy.shell.groups;
const ALERT_POLL_MS = 30_000;
const alertText = copy.overview.sidebar;

export const NAV_ITEMS: NavItem[] = [
	{
		label: copy.nav.overview,
		to: "/overview",
		icon: LayoutDashboard,
		group: workspace,
	},
	{ label: copy.nav.hosts, to: "/hosts", icon: Server, group: workspace },
	{
		label: copy.nav.policies,
		to: "/policies",
		icon: ShieldCheck,
		group: backup,
	},
	{
		label: copy.nav.jobs,
		to: "/jobs",
		icon: ListChecks,
		group: backup,
	},
	{
		label: copy.nav.restore,
		to: "/restore",
		icon: ArchiveRestore,
		group: backup,
	},
	{
		label: copy.nav.settings,
		pageLabel: copy.repos.title,
		to: "/settings/repositories",
		icon: Settings,
		group: system,
	},
];

const groups = [workspace, backup, system].map((label) => ({
	label,
	items: NAV_ITEMS.filter((item) => item.group === label),
}));

const themeOptions: { value: Theme; label: string; icon: LucideIcon }[] = [
	{ value: "light", label: copy.theme.light, icon: Sun },
	{ value: "dark", label: copy.theme.dark, icon: Moon },
	{ value: "system", label: copy.theme.system, icon: Monitor },
];

const sectionOf = (to: string) => `/${to.split("/")[1]}`;

const initialsOf = (name: string) =>
	name
		.split(/[\s._@-]+/)
		.filter(Boolean)
		.slice(0, 2)
		.map((part) => part[0].toUpperCase())
		.join("");

export function AppSidebar({ onOpenPalette }: { onOpenPalette: () => void }) {
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const version = useQuery(infoQuery).data?.version;
	const username = useQuery(sessionQuery).data?.username;
	const signOut = useSignOut();
	const { setOpenMobile } = useSidebar();
	const { theme, setTheme } = useTheme();
	const alerts = useCountAlerts({
		query: {
			refetchInterval: ALERT_POLL_MS,
			refetchIntervalInBackground: false,
		},
	});
	const unsilenced = alerts.isError ? 0 : (alerts.data?.unsilenced ?? 0);

	return (
		<Sidebar collapsible="icon">
			<SidebarHeader className="h-16 justify-center border-b border-sidebar-border px-4">
				<Link
					to="/hosts"
					className="flex items-center gap-2 rounded-md text-base font-semibold outline-none focus-visible:ring-2 focus-visible:ring-sidebar-ring"
				>
					<LogoMark className="size-7 shrink-0" />
					<span className="group-data-[collapsible=icon]:hidden">
						{copy.brand}
					</span>
				</Link>
			</SidebarHeader>
			<SidebarContent>
				<nav aria-label={copy.shell.navLabel} className="flex flex-col">
					{groups.map((group) => (
						<SidebarGroup key={group.label}>
							<SidebarGroupLabel>{group.label}</SidebarGroupLabel>
							<SidebarGroupContent>
								<SidebarMenu>
									{group.items.map(({ label, to, icon: Icon, soon }) => {
										const section = sectionOf(to);
										const count = to === "/overview" ? unsilenced : 0;
										const countLabel = alertText.label(count);
										const isActive =
											pathname === section ||
											pathname.startsWith(`${section}/`);
										return (
											<SidebarMenuItem key={to}>
												<SidebarMenuButton
													asChild
													isActive={isActive}
													tooltip={
														soon
															? `${label} · ${copy.soon}`
															: count > 0
																? `${label} · ${countLabel}`
																: label
													}
													className="data-[active=true]:bg-primary-subtle data-[active=true]:font-semibold data-[active=true]:text-primary-strong"
												>
													<Link to={to}>
														<Icon />
														<span>{label}</span>
													</Link>
												</SidebarMenuButton>
												{count > 0 && (
													<>
														<SidebarMenuBadge
															role="img"
															aria-label={countLabel}
															className="peer-data-[size=default]/menu-button:top-2.5"
														>
															{alertText.badge(count)}
														</SidebarMenuBadge>
														<span className="pointer-events-none absolute top-2 right-2 hidden size-2 rounded-full bg-destructive group-data-[collapsible=icon]:block">
															<span className="sr-only">{countLabel}</span>
														</span>
													</>
												)}
												{soon && (
													<SidebarMenuBadge className="text-muted-foreground peer-data-[size=default]/menu-button:top-2.5">
														{copy.soon}
													</SidebarMenuBadge>
												)}
											</SidebarMenuItem>
										);
									})}
								</SidebarMenu>
							</SidebarGroupContent>
						</SidebarGroup>
					))}
				</nav>
			</SidebarContent>
			<SidebarFooter>
				{username && (
					<DropdownMenu>
						<DropdownMenuTrigger asChild>
							<SidebarMenuButton className="h-12 group-data-[collapsible=icon]:p-0!">
								<Avatar aria-hidden>
									<AvatarFallback className="bg-primary-subtle text-sm font-semibold text-primary-strong">
										{initialsOf(username)}
									</AvatarFallback>
								</Avatar>
								<span className="truncate">{username}</span>
								<ChevronsUpDown className="ml-auto" />
							</SidebarMenuButton>
						</DropdownMenuTrigger>
						<DropdownMenuContent side="top" align="start" className="min-w-56">
							<DropdownMenuLabel className="flex flex-col">
								<span className="truncate text-base font-semibold text-foreground">
									{username}
								</span>
								{version && (
									<span className="font-mono text-sm font-normal text-muted-foreground">
										{`v${version}`}
									</span>
								)}
							</DropdownMenuLabel>
							<DropdownMenuSeparator />
							<DropdownMenuLabel>{copy.theme.label}</DropdownMenuLabel>
							<DropdownMenuRadioGroup
								value={theme}
								onValueChange={(value) => setTheme(value as Theme)}
							>
								{themeOptions.map(({ value, label, icon: Icon }) => (
									<DropdownMenuRadioItem key={value} value={value}>
										<Icon />
										{label}
									</DropdownMenuRadioItem>
								))}
							</DropdownMenuRadioGroup>
							<DropdownMenuSeparator />
							<DropdownMenuItem
								onSelect={() => {
									setOpenMobile(false);
									onOpenPalette();
								}}
							>
								{copy.shell.commandPalette}
								<ModK className="ml-auto" />
							</DropdownMenuItem>
							<DropdownMenuItem asChild>
								<a href="/api/docs" target="_blank" rel="noopener noreferrer">
									{copy.apiReference}
									<ExternalLink className="ml-auto" />
								</a>
							</DropdownMenuItem>
							<DropdownMenuItem onSelect={signOut}>
								{copy.signOut}
							</DropdownMenuItem>
						</DropdownMenuContent>
					</DropdownMenu>
				)}
			</SidebarFooter>
		</Sidebar>
	);
}
