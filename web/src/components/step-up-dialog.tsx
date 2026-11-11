import { type FormEvent, useEffect, useRef, useState } from "react";
import { ApiError, onStepUpRequired } from "@/api/fetcher";
import { useStepUp } from "@/api/gen/endpoints";
import { RowField } from "@/components/policies/form-field";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

type Waiting = { promise: Promise<boolean>; resolve: (ok: boolean) => void };

export function StepUpDialog() {
	const [open, setOpen] = useState(false);
	const [password, setPassword] = useState("");
	const [error, setError] = useState<string>();
	const waiting = useRef<Waiting>(undefined);
	const returnFocus = useRef<HTMLElement | null>(null);
	const stepUp = useStepUp({ mutation: { gcTime: 0 } });

	useEffect(() => {
		onStepUpRequired(() => {
			if (!waiting.current) {
				let resolve: (ok: boolean) => void = () => {};
				const promise = new Promise<boolean>((r) => {
					resolve = r;
				});
				waiting.current = { promise, resolve };
				const active = document.activeElement;
				returnFocus.current =
					active instanceof HTMLElement && active !== document.body
						? active
						: (Array.from(
								document.querySelectorAll<HTMLElement>(
									'[role="dialog"][data-state="open"]',
								),
							).at(-1) ?? null);
				setOpen(true);
			}
			return waiting.current.promise;
		});
		return () => onStepUpRequired(undefined);
	}, []);

	function settle(ok: boolean) {
		waiting.current?.resolve(ok);
		waiting.current = undefined;
		setOpen(false);
		setPassword("");
		setError(undefined);
		stepUp.reset();
	}

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (stepUp.isPending || !password) return;
		setError(undefined);
		stepUp.mutate(
			{ data: { password } },
			{
				onSuccess: () => settle(true),
				onError: (e) => {
					if (e.status === 401) {
						settle(false);
						return;
					}
					setPassword("");
					if (e.code === "wrong_password") {
						setError(copy.stepUp.wrong);
					} else if (e instanceof ApiError && e.status === 429) {
						setError(copy.login.rateLimited(e.retryAfter ?? 60));
					} else if (e instanceof ApiError && e.status === 503) {
						setError(copy.login.busy(e.retryAfter ?? 5));
					} else {
						setError(copy.errors.submit(copy.stepUp.action, e.message));
					}
				},
			},
		);
	}

	return (
		<Dialog
			open={open}
			onOpenChange={(next) => !next && !stepUp.isPending && settle(false)}
		>
			<DialogContent
				className="sm:max-w-[420px]"
				onCloseAutoFocus={(event) => {
					const target = returnFocus.current;
					returnFocus.current = null;
					if (!target?.isConnected) return;
					event.preventDefault();
					// The trigger is often disabled while its request waits; focus it once it settles.
					setTimeout(() => {
						target.focus();
						if (document.activeElement !== target) {
							target.closest<HTMLElement>('[role="dialog"]')?.focus();
						}
					}, 0);
				}}
			>
				<DialogHeader>
					<DialogTitle>{copy.stepUp.title}</DialogTitle>
					<DialogDescription>{copy.stepUp.body}</DialogDescription>
				</DialogHeader>
				<form onSubmit={onSubmit} noValidate>
					<RowField label={copy.stepUp.password} error={error}>
						{(field) => (
							<Input
								{...field}
								name="step-up-password"
								type="password"
								autoComplete="current-password"
								autoFocus
								disabled={stepUp.isPending}
								value={password}
								onChange={(e) => {
									setPassword(e.target.value);
									setError(undefined);
								}}
							/>
						)}
					</RowField>
					<DialogFooter className="mt-6">
						<Button
							type="button"
							variant="outline"
							disabled={stepUp.isPending}
							onClick={() => settle(false)}
						>
							{copy.stepUp.cancel}
						</Button>
						<Button type="submit" disabled={stepUp.isPending || !password}>
							{stepUp.isPending && <Spinner />}
							{copy.stepUp.confirm}
						</Button>
					</DialogFooter>
				</form>
			</DialogContent>
		</Dialog>
	);
}
