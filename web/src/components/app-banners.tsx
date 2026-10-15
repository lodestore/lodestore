import { useQuery } from "@tanstack/react-query";
import { TriangleAlert, X } from "lucide-react";
import { useState } from "react";
import {
	Alert,
	AlertAction,
	AlertDescription,
	AlertTitle,
} from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
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

export function AppBanners() {
	const info = useQuery(infoQuery);
	const { dismissed, dismiss } = useDismiss([UNLOCK_KEY, UNENCRYPTED]);

	const showUnlock =
		info.data?.unlock_key_source === "data_dir" && !dismissed.has(UNLOCK_KEY);
	const showUnencrypted =
		isInsecureOrigin(location.protocol, location.hostname) &&
		!dismissed.has(UNENCRYPTED);

	if (!showUnlock && !showUnencrypted) return null;
	return (
		<div className="mb-4 flex flex-col gap-4">
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
		</div>
	);
}
