import { CircleCheck, LoaderCircle, Square, TriangleAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { FindSession } from "@/api/gen/model";
import { TextWithPaths } from "@/components/path-text";
import { RetryAlert } from "@/components/restore/restore-states";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Progress } from "@/components/ui/progress";
import { copy } from "@/copy";
import { formatDuration } from "@/lib/format";

const ANNOUNCE_MS = 5000;

function useAnnouncement(text: string, immediate: boolean) {
	const [announced, setAnnounced] = useState(text);
	const last = useRef(Date.now());
	useEffect(() => {
		const wait = immediate
			? 0
			: Math.max(0, ANNOUNCE_MS - (Date.now() - last.current));
		const timer = setTimeout(() => {
			last.current = Date.now();
			setAnnounced(text);
		}, wait);
		return () => clearTimeout(timer);
	}, [text, immediate]);
	return announced;
}

function describe(session: FindSession) {
	const text = describeState(session);
	return session.skipped > 0 && session.state !== "failed"
		? `${text} · ${copy.find.skipped(session.skipped)}`
		: text;
}

function describeState(session: FindSession) {
	const duration = formatDuration(
		(session.finished_at ?? session.started_at) - session.started_at,
	);
	switch (session.state) {
		case "running":
			return copy.find.status.running(session);
		case "done":
			return copy.find.status.done({ ...session, duration });
		case "cancelled":
			return copy.find.status.cancelled;
		case "incomplete":
			switch (session.stop_reason) {
				case "timeout":
					return copy.find.status.timeout(duration);
				case "walk_limit":
					return copy.find.status.walkLimit;
				default:
					return copy.find.status.limit(session.limit);
			}
		case "failed":
			return copy.find.status.failed(session.error ?? "");
	}
}

export function FindStatus({
	session,
	error,
	onStop,
	onRetry,
}: {
	session: FindSession | undefined;
	error?: Error | null;
	onStop: () => void;
	onRetry: () => void;
}) {
	const text = session ? describe(session) : "";
	const final = session !== undefined && session.state !== "running";
	const announced = useAnnouncement(text, final);

	if (error && (!session || session.state === "running")) {
		return (
			<RetryAlert onRetry={onRetry}>
				<TextWithPaths text={copy.find.status.failed(error.message)} />
			</RetryAlert>
		);
	}
	if (!session) return null;

	const alerts = session.state === "failed" || session.state === "incomplete";
	return (
		<div className="flex flex-col gap-2">
			<div role="status" aria-live="polite" className="sr-only">
				{alerts ? "" : announced}
			</div>
			{session.state === "running" && (
				<>
					<div className="flex flex-wrap items-center justify-between gap-2">
						<p aria-hidden className="flex items-center gap-2 text-sm">
							<LoaderCircle
								aria-hidden
								className="size-4 shrink-0 animate-spin text-primary-strong motion-reduce:animate-none"
							/>
							{text}
						</p>
						<Button type="button" variant="outline" size="sm" onClick={onStop}>
							<Square aria-hidden />
							{copy.find.stop}
						</Button>
					</div>
					<Progress
						value={
							session.total > 0 ? (session.scanned / session.total) * 100 : null
						}
						aria-label={copy.find.status.searching}
						aria-valuetext={copy.find.status.progress(
							session.scanned,
							session.total,
						)}
					/>
				</>
			)}
			{session.state === "done" && (
				<p aria-hidden className="flex items-center gap-2 text-sm">
					<CircleCheck aria-hidden className="size-4 shrink-0 text-success" />
					{text}
				</p>
			)}
			{session.state === "cancelled" && (
				<p aria-hidden className="flex items-center gap-2 text-sm">
					<Square
						aria-hidden
						className="size-4 shrink-0 text-muted-foreground"
					/>
					{text}
				</p>
			)}
			{session.state === "incomplete" && (
				<Alert className="border-warning/60">
					<TriangleAlert aria-hidden className="text-warning" />
					<AlertDescription>{text}</AlertDescription>
				</Alert>
			)}
			{session.state === "failed" && (
				<RetryAlert onRetry={onRetry}>
					<TextWithPaths text={copy.find.status.failed(session.error ?? "")} />
				</RetryAlert>
			)}
		</div>
	);
}
