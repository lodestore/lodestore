import { Fragment } from "react";
import { PathText } from "@/components/path-text";
import {
	Breadcrumb,
	BreadcrumbEllipsis,
	BreadcrumbItem,
	BreadcrumbLink,
	BreadcrumbList,
	BreadcrumbPage,
	BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import {
	DropdownMenu,
	DropdownMenuContent,
	DropdownMenuItem,
	DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { decodeText, displayName, pathParts, urlPath } from "@/lib/names";

const MAX_LABEL = 24;
const COLLAPSE_AFTER = 4;

interface Crumb {
	label: string;
	target: string;
}

function shorten(label: string) {
	const chars = [...label];
	return chars.length <= MAX_LABEL
		? label
		: `${chars.slice(0, MAX_LABEL - 1).join("")}…`;
}

function WithTooltip({
	label,
	children,
}: {
	label: string;
	children: React.ReactElement;
}) {
	if (shorten(label) === label) return children;
	return (
		<Tooltip>
			<TooltipTrigger asChild>{children}</TooltipTrigger>
			<TooltipContent className="max-w-lg">
				<PathText value={label} />
			</TooltipContent>
		</Tooltip>
	);
}

export function PathBar({
	pathB64,
	onOpen,
}: {
	pathB64: string;
	onOpen: (target: string) => void;
}) {
	const parts = pathParts(pathB64);
	const crumbs: Crumb[] = [
		{ label: copy.restore.browser.root, target: "" },
		...parts.map((part, i) => ({
			label: displayName(decodeText(part)),
			target: urlPath(parts.slice(0, i + 1)),
		})),
	];
	const collapsed = parts.length > COLLAPSE_AFTER;
	const hidden = collapsed ? crumbs.slice(1, -2) : [];
	const shown = collapsed ? [crumbs[0], ...crumbs.slice(-2)] : crumbs;

	return (
		<Breadcrumb aria-label={copy.restore.browser.pathLabel} className="min-w-0">
			<BreadcrumbList className="flex-nowrap gap-2 sm:gap-2">
				{shown.map((crumb, i) => {
					const last = i === shown.length - 1;
					return (
						<Fragment key={crumb.target}>
							<BreadcrumbItem className="min-w-0">
								{last ? (
									<WithTooltip label={crumb.label}>
										<BreadcrumbPage className="font-mono">
											{shorten(crumb.label)}
										</BreadcrumbPage>
									</WithTooltip>
								) : (
									<WithTooltip label={crumb.label}>
										<BreadcrumbLink asChild>
											<button
												type="button"
												className="font-mono text-primary-strong hover:text-primary-strong hover:underline"
												onClick={() => onOpen(crumb.target)}
											>
												{shorten(crumb.label)}
											</button>
										</BreadcrumbLink>
									</WithTooltip>
								)}
							</BreadcrumbItem>
							{!last && <BreadcrumbSeparator />}
							{i === 0 && collapsed && (
								<>
									<BreadcrumbItem>
										<DropdownMenu>
											<DropdownMenuTrigger
												aria-label={copy.restore.browser.more}
												className="rounded-sm text-primary-strong"
											>
												<BreadcrumbEllipsis />
											</DropdownMenuTrigger>
											<DropdownMenuContent align="start">
												{hidden.map((item) => (
													<DropdownMenuItem
														key={item.target}
														className="font-mono"
														onSelect={() => onOpen(item.target)}
													>
														{item.label}
													</DropdownMenuItem>
												))}
											</DropdownMenuContent>
										</DropdownMenu>
									</BreadcrumbItem>
									<BreadcrumbSeparator />
								</>
							)}
						</Fragment>
					);
				})}
			</BreadcrumbList>
		</Breadcrumb>
	);
}
