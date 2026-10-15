import { Check, CircleAlert, CircleCheck, Copy, Download } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { commitWizard, startWizard } from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import {
	Field,
	FieldDescription,
	FieldError,
	FieldGroup,
	FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import { type Draft, lastGroup, toApiDraft } from "./draft";
import { SecretInput, StepCard, StepHeading } from "./parts";
import { text } from "./text";

const MIN_PASSWORD = 12;

type Started = { id: string; key: string };
type StartRequest = {
	payload: string;
	response: ReturnType<typeof startWizard>;
};

function download(name: string, key: string) {
	const url = URL.createObjectURL(
		new Blob([`${key}\n`], { type: "text/plain" }),
	);
	const link = document.createElement("a");
	link.href = url;
	link.download = `lodestore-recovery-key-${name}.txt`;
	document.body.append(link);
	link.click();
	link.remove();
	setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

function Section({
	title,
	done,
	children,
}: {
	title: string;
	done?: boolean;
	children: ReactNode;
}) {
	return (
		<Card className="gap-6 px-8 [--card-spacing:--spacing(8)]">
			<h3
				className="flex items-center gap-2 text-base font-semibold"
				data-done={done || undefined}
			>
				{done ? `${title} ` : title}
				{done && (
					<>
						<CircleCheck
							aria-hidden
							data-slot="done"
							className="size-5 text-success"
						/>
						<span className="sr-only">{copy.recoverySteps.completed}</span>
					</>
				)}
			</h3>
			{children}
		</Card>
	);
}

function tidy(message: string) {
	return message.replace(/\.$/, "");
}

export function RecoveryKeyStep({
	draft,
	onBack,
	onCreated,
}: {
	draft: Draft;
	onBack: () => void;
	onCreated: (repository: RepositorySummary) => void;
}) {
	const [started, setStarted] = useState<Started>();
	const [startError, setStartError] = useState<string>();
	const [password, setPassword] = useState("");
	const [confirm, setConfirm] = useState("");
	const [typed, setTyped] = useState("");
	const [pending, setPending] = useState(false);
	const [submitError, setSubmitError] = useState<string>();
	const [mismatch, setMismatch] = useState(false);

	const [copied, setCopied] = useState(false);
	const [downloaded, setDownloaded] = useState(false);

	const request = useRef<StartRequest>(undefined);
	const keyPanel = useRef<HTMLDivElement>(null);
	const created = useRef(false);

	useEffect(() => {
		if (created.current) return;
		const apiDraft = toApiDraft(draft);
		const payload = JSON.stringify(apiDraft);
		if (request.current?.payload !== payload) {
			request.current = { payload, response: startWizard(apiDraft) };
		}
		let cancelled = false;
		request.current.response.then(
			(response) => {
				if (cancelled) return;
				setStarted({ id: response.wizard_id, key: response.recovery_key });
			},
			(error: unknown) => {
				if (cancelled) return;
				setStartError(
					copy.errors.submit(
						text.keyAction,
						error instanceof ApiError ? tidy(error.message) : undefined,
					),
				);
			},
		);
		return () => {
			cancelled = true;
		};
	}, [draft]);

	useEffect(() => {
		if (!started) return;
		const warn = (event: BeforeUnloadEvent) => {
			event.preventDefault();
		};
		window.addEventListener("beforeunload", warn);
		return () => window.removeEventListener("beforeunload", warn);
	}, [started]);

	const group = started ? lastGroup(started.key) : "";
	const passwordLength = Array.from(password).length;
	const passwordOk = passwordLength >= MIN_PASSWORD;
	const confirmOk = confirm === password;
	const typedOk =
		group !== "" && typed.trim().toLowerCase() === group.toLowerCase();
	const showTypedError =
		mismatch ||
		(typed.trim().length >= group.length && typed !== "" && !typedOk);
	const ready = started !== undefined && passwordOk && confirmOk && typedOk;
	const parts = started?.key.split("-") ?? [];
	const prefix = parts.slice(0, 2).join("-");
	const groups = parts.slice(2);
	const createHint =
		!confirmOk || confirm === ""
			? text.createHint.confirm
			: text.createHint.lastGroup;
	const showHint = started !== undefined && passwordOk && !ready && !pending;

	async function create() {
		if (!started || !ready || pending) return;
		setPending(true);
		setSubmitError(undefined);
		setMismatch(false);
		try {
			const repository = await commitWizard(started.id, {
				password,
				recovery_key_last_group: typed.trim(),
			});
			created.current = true;
			setStarted(undefined);
			setPassword("");
			setConfirm("");
			setTyped("");
			onCreated(repository);
		} catch (error) {
			setPending(false);
			if (error instanceof ApiError && error.status === 422) {
				setMismatch(true);
				return;
			}
			setSubmitError(
				copy.errors.submit(
					text.createAction,
					error instanceof ApiError ? tidy(error.message) : undefined,
				),
			);
		}
	}

	return (
		<StepCard
			onBack={onBack}
			backDisabled={pending}
			actions={
				<>
					{showHint && (
						<p
							id="create-repository-hint"
							className="text-sm text-muted-foreground"
						>
							{createHint}
						</p>
					)}
					<Button
						type="button"
						disabled={!ready || pending}
						aria-describedby={
							showHint
								? "create-repository-hint"
								: started !== undefined && !passwordOk
									? "repository-password-help"
									: undefined
						}
						onClick={create}
					>
						{pending && <Spinner />}
						{copy.wizard.createRepository}
					</Button>
				</>
			}
		>
			<StepHeading>{text.stepHeading.key}</StepHeading>
			<p className="text-sm text-muted-foreground">{copy.recoveryKey.body}</p>

			{startError && (
				<Alert variant="destructive" role="alert">
					<CircleAlert aria-hidden />
					<AlertDescription>{startError}</AlertDescription>
				</Alert>
			)}
			{!started && !startError && (
				<p role="status" className="flex items-center gap-2 text-sm">
					<Spinner />
					{text.keyLoading}
				</p>
			)}

			{started && (
				<>
					<Section title={copy.recoverySteps.save}>
						<div
							ref={keyPanel}
							role="region"
							aria-label={text.keyObject}
							tabIndex={0}
							className="rounded-lg bg-muted p-6"
						>
							<span className="sr-only">{started.key}</span>
							<p
								aria-hidden
								className="mb-2 font-mono text-sm text-muted-foreground"
							>
								{prefix}
							</p>
							<div
								aria-hidden
								className="grid grid-cols-4 gap-x-6 gap-y-2 font-mono text-base font-normal tabular-nums tracking-[0.04em]"
							>
								{groups.map((part, index) => (
									<span
										key={index}
										className={
											index === groups.length - 1
												? "flex flex-col gap-1"
												: undefined
										}
									>
										<span
											className={
												index === groups.length - 1
													? "border-b-2 border-primary-strong"
													: undefined
											}
										>
											{part}
										</span>
										{index === groups.length - 1 && (
											<span className="font-sans text-sm tracking-normal text-muted-foreground">
												{copy.recoverySteps.lastGroup}
											</span>
										)}
									</span>
								))}
							</div>
						</div>
						<div className="flex flex-wrap items-center gap-4">
							<Button
								type="button"
								variant="outline"
								onClick={async () => {
									if (await copyText(started.key, keyPanel.current)) {
										setCopied(true);
									}
								}}
							>
								<Copy aria-hidden />
								{copy.recoveryKey.copy}
							</Button>
							{copied && (
								<span className="flex items-center gap-1 text-sm text-success">
									<Check aria-hidden className="size-4" />
									{copy.recoverySteps.copied}
								</span>
							)}
							<Button
								type="button"
								variant="outline"
								onClick={() => {
									download(draft.name, started.key);
									setDownloaded(true);
								}}
							>
								<Download aria-hidden />
								{copy.recoveryKey.download}
							</Button>
							{downloaded && (
								<span className="flex items-center gap-1 text-sm text-success">
									<Check aria-hidden className="size-4" />
									{copy.recoverySteps.downloaded}
								</span>
							)}
						</div>
						<p className="text-sm text-muted-foreground">
							{copy.recoverySteps.hint}
						</p>
					</Section>

					<Section
						title={copy.recoverySteps.password}
						done={passwordOk && confirmOk && confirm !== ""}
					>
						<FieldGroup>
							<Field>
								<FieldLabel htmlFor="repository-password">
									{copy.recoveryKey.passwordLabel}
								</FieldLabel>
								<SecretInput
									id="repository-password"
									what="repository password"
									autoComplete="new-password"
									describedBy="repository-password-help"
									value={password}
									onChange={setPassword}
								/>
								<FieldDescription id="repository-password-help">
									{copy.recoveryKey.passwordHelper}
								</FieldDescription>
							</Field>
							<Field data-invalid={(confirm !== "" && !confirmOk) || undefined}>
								<FieldLabel htmlFor="repository-password-confirm">
									{text.confirmPassword}
								</FieldLabel>
								<SecretInput
									id="repository-password-confirm"
									what="password confirmation"
									autoComplete="new-password"
									invalid={confirm !== "" && !confirmOk}
									describedBy="repository-password-confirm-error"
									value={confirm}
									onChange={setConfirm}
								/>
								{confirm !== "" && !confirmOk && (
									<FieldError id="repository-password-confirm-error">
										{text.passwordMismatch}
									</FieldError>
								)}
							</Field>
						</FieldGroup>
					</Section>

					<Section title={copy.recoverySteps.confirm} done={typedOk}>
						<Field data-invalid={showTypedError || undefined}>
							<FieldLabel htmlFor="recovery-key-last-group">
								{copy.recoveryKey.confirmLabel}
							</FieldLabel>
							<Input
								id="recovery-key-last-group"
								name="recovery-key-last-group"
								className="font-mono"
								autoComplete="off"
								autoCapitalize="off"
								spellCheck={false}
								aria-invalid={showTypedError || undefined}
								aria-describedby="recovery-key-last-group-help"
								value={typed}
								onChange={(event) => {
									setTyped(event.target.value);
									setMismatch(false);
								}}
							/>
							{showTypedError ? (
								<FieldError id="recovery-key-last-group-help">
									{copy.recoveryKey.mismatch}
								</FieldError>
							) : (
								<FieldDescription id="recovery-key-last-group-help">
									{copy.recoveryKey.confirmHelper}
								</FieldDescription>
							)}
						</Field>
					</Section>
				</>
			)}

			{submitError && (
				<Alert variant="destructive" role="alert">
					<CircleAlert aria-hidden />
					<AlertDescription>{submitError}</AlertDescription>
				</Alert>
			)}
		</StepCard>
	);
}
