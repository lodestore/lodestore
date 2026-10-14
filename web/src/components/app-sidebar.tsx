import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useRouterState } from "@tanstack/react-router";
import {
	ArchiveRestore,
	ChevronsUpDown,
	LayoutDashboard,
	ListChecks,
	type LucideIcon,
	Server,
	Settings,
	ShieldCheck,
} from "lucide-react";
import { toast } from "sonner";
import { setCsrfToken } from "@/api/fetcher";
import { useLogout } from "@/api/gen/endpoints";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Sidebar,
	SidebarContent,
	SidebarFooter,
	SidebarGroup,
	SidebarGroupContent,
	SidebarHeader,
	SidebarMenu,
	SidebarMenuBadge,
	SidebarMenuButton,
	SidebarMenuItem,
} from "@/components/ui/sidebar";
import { copy } from "@/copy";
import { infoQuery, sessionQuery } from "@/lib/session";

const items: {
	label: string;
	to: string;
	icon: LucideIcon;
	soon?: boolean;
}[] = [
	{
		label: copy.nav.overview,
		to: "/overview",
		icon: LayoutDashboard,
		soon: true,
	},
	{ label: copy.nav.hosts, to: "/hosts", icon: Server },
	{ label: copy.nav.policies, to: "/policies", icon: ShieldCheck, soon: true },
	{ label: copy.nav.jobs, to: "/jobs", icon: ListChecks, soon: true },
	{ label: copy.nav.restore, to: "/restore", icon: ArchiveRestore, soon: true },
	{ label: copy.nav.settings, to: "/settings/repositories", icon: Settings },
];

const sectionOf = (to: string) => `/${to.split("/")[1]}`;

export function AppSidebar() {
	const pathname = useRouterState({ select: (s) => s.location.pathname });
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const version = useQuery(infoQuery).data?.version;
	const username = useQuery(sessionQuery).data?.username;
	const logout = useLogout();

	function signOut() {
		logout.mutate(undefined, {
			onSuccess: async () => {
				setCsrfToken(undefined);
				queryClient.removeQueries({ queryKey: sessionQuery.queryKey });
				await navigate({
					to: "/login",
					search: { next: undefined, expired: undefined },
				});
				queryClient.clear();
			},
			onError: (e) => toast.error(copy.errors.submit("sign out", e.message)),
		});
	}

	return (
		<Sidebar collapsible="icon">
			<SidebarHeader className="px-4 py-4 text-lg leading-6 font-semibold group-data-[collapsible=icon]:hidden">
				Lodestore
			</SidebarHeader>
			<SidebarContent>
				<SidebarGroup>
					<SidebarGroupContent>
						<SidebarMenu>
							{items.map(({ label, to, icon: Icon, soon }) => {
								const section = sectionOf(to);
								const isActive =
									pathname === section || pathname.startsWith(`${section}/`);
								return (
									<SidebarMenuItem key={to}>
										<SidebarMenuButton
											asChild
											isActive={isActive}
											tooltip={label}
											className="data-[active=true]:font-semibold data-[active=true]:shadow-[inset_2px_0_0_var(--primary)]"
										>
											<Link to={to}>
												<Icon />
												<span>{label}</span>
											</Link>
										</SidebarMenuButton>
										{soon && (
											<SidebarMenuBadge className="text-muted-foreground">
												{copy.soon}
											</SidebarMenuBadge>
										)}
									</SidebarMenuItem>
								);
							})}
						</SidebarMenu>
					</SidebarGroupContent>
				</SidebarGroup>
			</SidebarContent>
			<SidebarFooter>
				{version && (
					<span className="px-2 font-mono text-xs group-data-[collapsible=icon]:hidden">
						v{version}
					</span>
				)}
				{username && (
					<DropdownMenu>
						<DropdownMenuTrigger asChild>
							<SidebarMenuButton>
								<span className="truncate">{username}</span>
								<ChevronsUpDown className="ml-auto" />
							</SidebarMenuButton>
						</DropdownMenuTrigger>
						<DropdownMenuContent side="top" align="start">
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
