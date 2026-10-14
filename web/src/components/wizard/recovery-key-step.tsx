import { CircleAlert, Download } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { commitWizard, startWizard } from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { CodeBlock } from "@/components/code-block";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
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
	URL.revokeObjectURL(url);
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

	const request = useRef<StartRequest>(undefined);

	useEffect(() => {
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
				<Button type="button" disabled={!ready || pending} onClick={create}>
					{pending && <Spinner />}
					{copy.wizard.createRepository}
				</Button>
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
					<div className="flex flex-col gap-2">
						<CodeBlock
							text={started.key}
							object={text.keyObject}
							copyLabel={copy.recoveryKey.copy}
							large
						/>
						<div className="flex flex-wrap items-center gap-3">
							<Button
								type="button"
								variant="secondary"
								size="sm"
								onClick={() => download(draft.name, started.key)}
							>
								<Download aria-hidden />
								{copy.recoveryKey.download}
							</Button>
							<p className="text-sm text-muted-foreground">
								{copy.recoveryKey.goingBack}
							</p>
						</div>
					</div>

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
					</FieldGroup>
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
