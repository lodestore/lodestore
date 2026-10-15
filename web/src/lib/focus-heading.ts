import { useRouter } from "@tanstack/react-router";
import { useEffect } from "react";

const GIVE_UP_MS = 3000;
const findHeading = () =>
	document.querySelector<HTMLElement>("[data-page-title]") ??
	document.querySelector<HTMLElement>("h1");

function focusIsClaimed() {
	const active = document.activeElement;
	return (
		active instanceof HTMLElement &&
		active !== document.body &&
		active.closest('[data-state="closed"]') === null &&
		(active.closest("main") !== null || active.matches("input,textarea"))
	);
}

function focusHeading(heading: HTMLElement) {
	if (focusIsClaimed()) return;
	heading.tabIndex = -1;
	heading.focus({ preventScroll: true });
}

export function useFocusHeading() {
	const router = useRouter();
	useEffect(() => {
		let stopWaiting = () => {};
		function run() {
			stopWaiting();
			const existing = findHeading();
			if (existing) return focusHeading(existing);
			const observer = new MutationObserver(() => {
				const heading = findHeading();
				if (!heading) return;
				stopWaiting();
				focusHeading(heading);
			});
			observer.observe(document.body, { childList: true, subtree: true });
			const timer = setTimeout(() => stopWaiting(), GIVE_UP_MS);
			stopWaiting = () => {
				observer.disconnect();
				clearTimeout(timer);
			};
		}
		const unsubscribe = router.subscribe("onRendered", (event) => {
			if (event.pathChanged && event.fromLocation) run();
		});
		return () => {
			unsubscribe();
			stopWaiting();
		};
	}, [router]);
}
