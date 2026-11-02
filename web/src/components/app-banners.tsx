import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useMatchRoute, useNavigate } from "@tanstack/react-router";
import { Info, TriangleAlert, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
	getListRestoreHostsQueryKey,
	getListRestoreSnapshotsQueryKey,
} from "@/api/gen/endpoints";
import type { CatalogStatus } from "@/api/gen/model";
import {
	isActivePhase,
	isRebuilding,
	useCatalogStatus,
} from "@/components/repos/catalog-card";
import {
	Alert,
	AlertAction,
	AlertDescription,
	AlertTitle,
} from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { isInsecureOrigin } from "@/lib/insecure";
import { infoQuery } from "@/lib/session";

const UNLOCK_KEY = "banner-dismissed:unlock-key";
const UNENCRYPTED = "banner-dismissed:unencrypted";

function Banner({
	title,
	dismissLabel,
	children,
	onDismiss,
}: {
	title?: string;
	dismissLabel: string;
	children: string;
	onDismiss: () => void;
}) {
	return (
		<Alert className="border-warning/60">
			<TriangleAlert aria-hidden className="text-warning" />
			{title && <AlertTitle>{title}</AlertTitle>}
			<AlertDescription className="text-foreground">
				{children}
			</AlertDescription>
			<AlertAction>
				<Button
					variant="ghost"
					size="icon-xs"
					aria-label={dismissLabel}
					onClick={onDismiss}
				>
					<X aria-hidden />
				</Button>
			</AlertAction>
		</Alert>
	);
}

function readDismissed(key: string) {
	try {
		return sessionStorage.getItem(key) !== null;
	} catch {
		return false;
	}
}

function useDismiss(keys: string[]) {
	const [dismissed, setDismissed] = useState(
		() => new Set(keys.filter(readDismissed)),
	);
	const dismiss = (key: string) => {
		try {
			sessionStorage.setItem(key, "1");
		} catch {
			// Blocked storage: the banner is still hidden for this page view.
		}
		setDismissed((current) => new Set(current).add(key));
	};
	return { dismissed, dismiss };
}

export function UnencryptedBanner() {
	const { dismissed, dismiss } = useDismiss([UNENCRYPTED]);
	if (
		!isInsecureOrigin(location.protocol, location.hostname) ||
		dismissed.has(UNENCRYPTED)
	)
		return null;
	return (
		<Banner
			title={copy.unencryptedBanner.title}
			dismissLabel={copy.unencryptedBanner.dismiss}
			onDismiss={() => dismiss(UNENCRYPTED)}
		>
			{copy.unencryptedBanner.body}
		</Banner>
	);
}

const REPORT_TOAST_MS = 8_000;
const DONE_TOAST_MS = 4_000;

function useRebuildNotices(statuses: CatalogStatus[] | undefined) {
	const queryClient = useQueryClient();
	const navigate = useNavigate();
	const seen = useRef(new Map<string, CatalogStatus["phase"]>());

	useEffect(() => {
		for (const status of statuses ?? []) {
			const was = seen.current.get(status.repository_id);
			seen.current.set(status.repository_id, status.phase);
			if (!isActivePhase(was) || isActivePhase(status.phase)) continue;
			void queryClient.invalidateQueries({
				queryKey: getListRestoreSnapshotsQueryKey(),
			});
			void queryClient.invalidateQueries({
				queryKey: getListRestoreHostsQueryKey(),
			});
			const report = () =>
				void navigate({
					to: "/settings/repositories/$id",
					params: { id: status.repository_id },
				});
			if (status.phase === "failed") {
				toast.error(copy.catalog.failed(status.error), {
					action: { label: copy.catalog.viewReport, onClick: report },
				});
			} else if (status.skipped.length > 0) {
				toast.success(copy.catalog.done(status.snapshots), {
					duration: REPORT_TOAST_MS,
					action: { label: copy.catalog.viewReport, onClick: report },
				});
			} else {
				toast.success(copy.catalog.done(status.snapshots), {
					duration: DONE_TOAST_MS,
				});
			}
		}
	}, [statuses, queryClient, navigate]);
}

export function RebuildBanner() {
	const statuses = useCatalogStatus();
	const matchRoute = useMatchRoute();
	useRebuildNotices(statuses.data);
	return (statuses.data ?? []).filter(isRebuilding).map((status) => (
		<Alert key={status.repository_id} role="note">
			<Spinner aria-hidden />
			<AlertDescription className="flex flex-wrap items-center gap-x-4 text-foreground">
				{status.phase === "listing" || !status.total
					? copy.catalog.bannerListing(status.repository_name)
					: copy.catalog.banner(
							status.repository_name,
							status.read,
							status.total,
						)}
				{!matchRoute({
					to: "/settings/repositories/$id",
					params: { id: status.repository_id },
				}) && (
					<Link
						to="/settings/repositories/$id"
						params={{ id: status.repository_id }}
						className="text-primary-strong underline"
					>
						{copy.catalog.viewProgress}
					</Link>
				)}
			</AlertDescription>
		</Alert>
	));
}

function DemoBanner() {
	return (
		<Alert role="note">
			<Info aria-hidden />
			<AlertDescription className="text-foreground">
				{copy.demo.banner}
			</AlertDescription>
		</Alert>
	);
}

export function AppBanners() {
	const info = useQuery(infoQuery);
	const { dismissed, dismiss } = useDismiss([UNLOCK_KEY, UNENCRYPTED]);

	const showUnlock =
		info.data?.unlock_key_source === "data_dir" && !dismissed.has(UNLOCK_KEY);
	const showUnencrypted =
		isInsecureOrigin(location.protocol, location.hostname) &&
		!dismissed.has(UNENCRYPTED);

	return (
		<div className="mb-4 flex flex-col gap-4 empty:hidden">
			{info.data?.demo_mode && <DemoBanner />}
			{showUnlock && (
				<Banner
					dismissLabel={copy.unlockKeyDismiss}
					onDismiss={() => dismiss(UNLOCK_KEY)}
				>
					{copy.unlockKeyBanner}
				</Banner>
			)}
			{showUnencrypted && (
				<Banner
					title={copy.unencryptedBanner.title}
					dismissLabel={copy.unencryptedBanner.dismiss}
					onDismiss={() => dismiss(UNENCRYPTED)}
				>
					{copy.unencryptedBanner.body}
				</Banner>
			)}
			<RebuildBanner />
		</div>
	);
}
