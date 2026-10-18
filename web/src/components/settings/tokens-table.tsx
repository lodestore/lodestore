import { Clock, MoreHorizontal, TriangleAlert } from "lucide-react";
import type { ApiTokenItem } from "@/api/gen/model";
import { Illustration } from "@/components/brand/illustration";
import { formatRelative, RelativeTime } from "@/components/relative-time";
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
import {
	Table,
	TableBody,
	TableCaption,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
} from "@/components/ui/table";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
	Tooltip,
	TooltipContent,
	TooltipTrigger,
} from "@/components/ui/tooltip";
import { copy } from "@/copy";
import { tokenCount } from "@/lib/plural";
import { cn } from "@/lib/utils";

const DAY_MS = 86_400_000;
const WARN_DAYS = 7;
const MAX_CHIPS = 2;
const SHOW_DETAIL = "hidden @min-[640px]:table-cell";
const SHOW_LAST_USED = "hidden @min-[880px]:table-cell";
const text = copy.tokens;

const absolute = (ms: number) =>
	new Date(ms).toLocaleString("en", {
		dateStyle: "medium",
		timeStyle: "medium",
	});

function Truncated({
	value,
	className,
}: {
	value: string;
	className?: string;
}) {
	return (
		<Tooltip>
			<TooltipTrigger asChild>
				<span tabIndex={0} className={cn("block truncate", className)}>
					{value}
				</span>
			</TooltipTrigger>
			<TooltipContent className="wrap-anywhere">{value}</TooltipContent>
		</Tooltip>
	);
}

function ScopeChips({ token }: { token: ApiTokenItem }) {
	const rest = token.scopes.slice(MAX_CHIPS);
	return (
		<div
			role="group"
			aria-label={text.scopesAria(token.scopes.join(", "))}
			className="flex items-center gap-1"
		>
			{token.scopes.slice(0, MAX_CHIPS).map((scope) => (
				<Badge
					key={scope}
					variant="outline"
					className="h-6 min-w-0 font-mono text-sm"
				>
					<span className="truncate">{scope}</span>
				</Badge>
			))}
			{rest.length > 0 && (
				<Tooltip>
					<TooltipTrigger asChild>
						<Badge variant="secondary" tabIndex={0} className="h-6 text-sm">
							+{rest.length}
						</Badge>
					</TooltipTrigger>
					<TooltipContent>
						<ul className="font-mono">
							{rest.map((scope) => (
								<li key={scope}>{scope}</li>
							))}
						</ul>
					</TooltipContent>
				</Tooltip>
			)}
		</div>
	);
}

function expiryOf(token: ApiTokenItem, now: Date) {
	if (token.revoked_at != null) {
		return {
			compact: text.compactRevoked(
				formatRelative(new Date(token.revoked_at), now),
			),
			cell: (
				<span className="text-muted-foreground">
					{text.revokedAt(formatRelative(new Date(token.revoked_at), now))}
				</span>
			),
		};
	}
	if (token.expires_at == null) {
		return {
			compact: text.compactNever,
			cell: <span className="text-muted-foreground">{text.neverExpires}</span>,
		};
	}
	const remaining = token.expires_at - now.getTime();
	if (remaining <= 0) {
		return {
			compact: text.compactExpired,
			cell: (
				<Badge variant="outline" className="h-6 text-sm text-neutral">
					<Clock aria-hidden />
					{text.expired}
				</Badge>
			),
		};
	}
	const days = Math.ceil(remaining / DAY_MS);
	const warn = days <= WARN_DAYS;
	return {
		compact: text.compactExpiresIn(days),
		cell: (
			<Tooltip>
				<TooltipTrigger asChild>
					<span
						tabIndex={0}
						className={cn(
							"inline-flex items-center gap-1",
							warn && "text-warning",
						)}
					>
						{warn && <TriangleAlert aria-hidden className="size-4" />}
						{text.expiresIn(days)}
					</span>
				</TooltipTrigger>
				<TooltipContent>{absolute(token.expires_at)}</TooltipContent>
			</Tooltip>
		),
	};
}

function SkeletonRows() {
	return Array.from({ length: 5 }, (_, i) => (
		<TableRow key={i} aria-hidden data-testid="skeleton-row" className="h-16">
			<TableCell className="px-4">
				<Skeleton className="h-5 w-40" />
				<Skeleton className="mt-2 h-4 w-24" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_DETAIL)}>
				<Skeleton className="h-6 w-48" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_DETAIL)}>
				<Skeleton className="h-5 w-24" />
			</TableCell>
			<TableCell className={cn("px-4", SHOW_LAST_USED)}>
				<Skeleton className="h-5 w-20" />
			</TableCell>
			<TableCell className="px-4">
				<Skeleton className="size-10" />
			</TableCell>
		</TableRow>
	));
}

export type TokenFilter = "active" | "revoked";

function RowActions({
	token,
	onRevoke,
}: {
	token: ApiTokenItem;
	onRevoke: (token: ApiTokenItem) => void;
}) {
	return (
		<DropdownMenu modal={false}>
			<DropdownMenuTrigger asChild>
				<Button
					variant="ghost"
					size="icon"
					data-token-actions={token.id}
					aria-label={text.actionsFor(token.name)}
				>
					<MoreHorizontal aria-hidden />
				</Button>
			</DropdownMenuTrigger>
			<DropdownMenuContent align="end">
				<DropdownMenuItem
					variant="destructive"
					onSelect={() => onRevoke(token)}
				>
					{text.revokeAction}
				</DropdownMenuItem>
			</DropdownMenuContent>
		</DropdownMenu>
	);
}

function EmptyTokens({
	filter,
	onCreate,
}: {
	filter: TokenFilter;
	onCreate: () => void;
}) {
	if (filter === "revoked") {
		return (
			<Empty>
				<EmptyHeader>
					<EmptyDescription>{text.emptyRevoked}</EmptyDescription>
				</EmptyHeader>
			</Empty>
		);
	}
	return (
		<Empty className="py-24">
			<Illustration
				name="repos-empty"
				width={320}
				height={216}
				className="h-auto w-80 max-w-full"
			/>
			<EmptyHeader>
				<EmptyTitle>{text.emptyHeading}</EmptyTitle>
				<EmptyDescription>{text.emptyBody}</EmptyDescription>
			</EmptyHeader>
			<EmptyContent>
				<Button onClick={onCreate}>{text.create}</Button>
			</EmptyContent>
		</Empty>
	);
}

export function TokensTable({
	tokens,
	filter,
	isError,
	onFilterChange,
	onRetry,
	onCreate,
	onRevoke,
}: {
	tokens?: ApiTokenItem[];
	filter: TokenFilter;
	isError: boolean;
	onFilterChange: (filter: TokenFilter) => void;
	onRetry: () => void;
	onCreate: () => void;
	onRevoke: (token: ApiTokenItem) => void;
}) {
	const now = new Date();
	return (
		<div className="flex flex-col gap-4">
			<div className="flex justify-end">
				<ToggleGroup
					type="single"
					variant="outline"
					size="lg"
					spacing={0}
					aria-label={text.filterLabel}
					value={filter}
					onValueChange={(value) => {
						if (value === "active" || value === "revoked")
							onFilterChange(value);
					}}
				>
					<ToggleGroupItem
						value="active"
						className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
					>
						{text.filterActive}
					</ToggleGroupItem>
					<ToggleGroupItem
						value="revoked"
						className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:text-primary-strong"
					>
						{text.filterRevoked}
					</ToggleGroupItem>
				</ToggleGroup>
			</div>
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
			{tokens?.length === 0 && (
				<EmptyTokens filter={filter} onCreate={onCreate} />
			)}
			{(tokens === undefined ? !isError : tokens.length > 0) && (
				<div className="@container overflow-hidden rounded-lg border bg-card">
					<Table>
						{tokens && (
							<TableCaption className="sr-only">
								{tokenCount(tokens.length)}
							</TableCaption>
						)}
						<TableHeader className="bg-muted">
							<TableRow>
								<TableHead className="px-4">{text.columns.name}</TableHead>
								<TableHead className={cn("px-4", SHOW_DETAIL)}>
									{text.columns.scopes}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_DETAIL)}>
									{text.columns.expires}
								</TableHead>
								<TableHead className={cn("px-4", SHOW_LAST_USED)}>
									{text.columns.lastUsed}
								</TableHead>
								<TableHead className="w-16 px-4">
									<span className="sr-only">{text.actionsColumn}</span>
								</TableHead>
							</TableRow>
						</TableHeader>
						<TableBody>
							{tokens === undefined ? (
								<SkeletonRows />
							) : (
								tokens.map((token) => {
									const expiry = expiryOf(token, now);
									const inactive =
										token.revoked_at != null ||
										(token.expires_at != null &&
											token.expires_at <= now.getTime());
									return (
										<TableRow
											key={token.id}
											data-token-row={token.id}
											tabIndex={-1}
											className={cn(
												"h-16 outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset",
												inactive && "text-muted-foreground",
											)}
										>
											<TableCell className="px-4">
												<Truncated
													value={token.name}
													className="max-w-40 text-base font-semibold @min-[880px]:max-w-56"
												/>
												<div className="hidden font-mono text-sm text-muted-foreground @min-[640px]:block">
													{token.prefix}…
												</div>
												<div className="text-sm text-muted-foreground @min-[640px]:hidden">
													{text.compactSummary(
														token.scopes.length,
														expiry.compact,
													)}
												</div>
											</TableCell>
											<TableCell className={cn("px-4", SHOW_DETAIL)}>
												<ScopeChips token={token} />
											</TableCell>
											<TableCell className={cn("px-4", SHOW_DETAIL)}>
												{expiry.cell}
											</TableCell>
											<TableCell className={cn("px-4", SHOW_LAST_USED)}>
												{token.last_used_at != null ? (
													<RelativeTime value={new Date(token.last_used_at)} />
												) : (
													<span className="text-muted-foreground">
														{text.neverUsed}
													</span>
												)}
											</TableCell>
											<TableCell className="px-4">
												{token.revoked_at == null && (
													<RowActions token={token} onRevoke={onRevoke} />
												)}
											</TableCell>
										</TableRow>
									);
								})
							)}
						</TableBody>
					</Table>
				</div>
			)}
		</div>
	);
}
