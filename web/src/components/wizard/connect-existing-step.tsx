import { CircleAlert, Info } from "lucide-react";
import { useState } from "react";
import { ApiError } from "@/api/fetcher";
import { connectRepository } from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Spinner } from "@/components/ui/spinner";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";
import { type Draft, toApiDraft } from "./draft";
import { SecretInput, StepCard, StepHeading } from "./parts";
import { text } from "./text";

type Method = "password" | "recovery_key";

export function ConnectExistingStep({
	draft,
	onBack,
	onConnected,
}: {
	draft: Draft;
	onBack: () => void;
	onConnected: (repository: RepositorySummary) => void;
}) {
	const [method, setMethod] = useState<Method>("password");
	const [password, setPassword] = useState("");
	const [recoveryKey, setRecoveryKey] = useState("");
	const [pending, setPending] = useState(false);
	const [error, setError] = useState<string>();

	const value = method === "password" ? password : recoveryKey;

	async function connect() {
		if (value.trim() === "" || pending) return;
		setPending(true);
		setError(undefined);
		try {
			// POST /api/v1/repositories/connect
			const repository = await connectRepository({
				draft: toApiDraft(draft),
				...(method === "password"
					? { password }
					: { recovery_key: recoveryKey.trim() }),
			});
			setPassword("");
			setRecoveryKey("");
			onConnected(repository);
		} catch (cause) {
			setPending(false);
			if (cause instanceof ApiError && cause.status === 422) {
				setError(copy.existingRepo.error);
				return;
			}
			setError(
				copy.errors.submit(
					text.connectAction,
					cause instanceof ApiError
						? cause.message.replace(/\.$/, "")
						: undefined,
				),
			);
		}
	}

	return (
		<StepCard
			onBack={onBack}
			backDisabled={pending}
			actions={
				<Button
					type="button"
					disabled={value.trim() === "" || pending}
					onClick={connect}
				>
					{pending && <Spinner />}
					{copy.existingRepo.button}
				</Button>
			}
		>
			<StepHeading>{text.connectHeading}</StepHeading>
			<Alert>
				<Info aria-hidden />
				<AlertTitle>{copy.existingRepo.heading}</AlertTitle>
				<AlertDescription>{copy.existingRepo.body}</AlertDescription>
			</Alert>
			<Tabs
				value={method}
				onValueChange={(next) => {
					setMethod(next as Method);
					setError(undefined);
				}}
			>
				<TabsList>
					<TabsTrigger value="password">
						{copy.existingRepo.tabPassword}
					</TabsTrigger>
					<TabsTrigger value="recovery_key">
						{copy.existingRepo.tabRecoveryKey}
					</TabsTrigger>
				</TabsList>
				<TabsContent value="password">
					<Field>
						<FieldLabel htmlFor="connect-password">
							{text.connectPasswordLabel}
						</FieldLabel>
						<SecretInput
							id="connect-password"
							what="repository password"
							autoComplete="current-password"
							disabled={pending}
							value={password}
							onChange={setPassword}
						/>
					</Field>
				</TabsContent>
				<TabsContent value="recovery_key">
					<Field>
						<FieldLabel htmlFor="connect-recovery-key">
							{text.connectKeyLabel}
						</FieldLabel>
						<SecretInput
							id="connect-recovery-key"
							what="recovery key"
							disabled={pending}
							value={recoveryKey}
							onChange={setRecoveryKey}
						/>
					</Field>
				</TabsContent>
			</Tabs>
			{error && (
				<Alert variant="destructive" role="alert">
					<CircleAlert aria-hidden />
					<AlertDescription>{error}</AlertDescription>
				</Alert>
			)}
		</StepCard>
	);
}
