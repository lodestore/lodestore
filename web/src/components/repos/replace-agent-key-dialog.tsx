import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert, CircleCheck } from "lucide-react";
import { type FormEvent, useState } from "react";
import {
	getGetRepositoryQueryKey,
	getListRepositoriesQueryKey,
	useListHosts,
	useReplaceKey,
	useVerifyKey,
} from "@/api/gen/endpoints";
import type { KeyReplaced } from "@/api/gen/model";
import { agentKeyWarning } from "@/components/capability-checklist";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { offlineAgents, onlineAgents } from "@/lib/plural";

type Step = "closed" | "form" | "confirm";

export function ReplaceAgentKeyDialog({
	repositoryId,
	name,
}: {
	repositoryId: string;
	name: string;
}) {
	const queryClient = useQueryClient();
	const [step, setStep] = useState<Step>("closed");
	const [accessKeyId, setAccessKeyId] = useState("");
	const [secret, setSecret] = useState("");
	const [error, setError] = useState<string>();
	const [keyWarningHeading, setKeyWarningHeading] = useState<string>();
	const [result, setResult] = useState<KeyReplaced>();

	// POST /api/v1/repositories/{id}/agent-key/verify, then PUT /api/v1/repositories/{id}/agent-key
	const verify = useVerifyKey({ mutation: { gcTime: 0 } });
	const replace = useReplaceKey({ mutation: { gcTime: 0 } });
	const hosts = useListHosts({ query: { enabled: step !== "closed" } });
	const online = (hosts.data ?? []).filter(
		(host) => host.repository_id === repositoryId && host.status === "online",
	).length;
	const busy = verify.isPending || replace.isPending;

	function wipe() {
		setSecret("");
		verify.reset();
		replace.reset();
	}

	function close() {
		wipe();
		setAccessKeyId("");
		setError(undefined);
		setStep("closed");
	}

	function onVerify(event: FormEvent) {
		event.preventDefault();
		if (busy) return;
		setError(undefined);
		verify.mutate(
			{
				id: repositoryId,
				data: { access_key_id: accessKeyId, secret_access_key: secret },
			},
			{
				onSuccess: ({ check, agent_key_state }) => {
					if (check.status === "fail") {
						setSecret("");
						setError(
							copy.repos.replaceKeyFailure(check.error ?? copy.common.dash),
						);
						return;
					}
					setKeyWarningHeading(
						agentKeyWarning({ checks: [check], agent_key_state })?.heading,
					);
					setStep("confirm");
				},
				onError: (e) => {
					setSecret("");
					setError(copy.errors.submit(copy.repos.keyChangeFailed, e.message));
				},
			},
		);
	}

	function onReplace() {
		replace.mutate(
			{
				id: repositoryId,
				data: { access_key_id: accessKeyId, secret_access_key: secret },
			},
			{
				onSuccess: async (replaced) => {
					close();
					setResult(replaced);
					await Promise.all([
						queryClient.invalidateQueries({
							queryKey: getGetRepositoryQueryKey(repositoryId),
						}),
						queryClient.invalidateQueries({
							queryKey: getListRepositoriesQueryKey(),
						}),
					]);
				},
				onError: (e) => {
					setSecret("");
					setError(
						e.status === 422
							? e.message
							: copy.errors.submit(copy.repos.keyChangeFailed, e.message),
					);
					setStep("form");
				},
			},
		);
	}

	return (
		<>
			<div className="flex flex-col items-start gap-4">
				<Button
					type="button"
					variant="secondary"
					onClick={() => {
						setResult(undefined);
						setStep("form");
					}}
				>
					{copy.repos.replaceKey}
				</Button>

				{result && (
					<Alert>
						<CircleCheck aria-hidden />
						<AlertDescription>
							{copy.repos.replaceKeyResult(
								onlineAgents(result.online_notified),
								offlineAgents(result.offline),
							)}
						</AlertDescription>
					</Alert>
				)}
			</div>

			<Dialog open={step === "form"} onOpenChange={(open) => !open && close()}>
				<DialogContent className="sm:max-w-[480px]">
					<DialogHeader>
						<DialogTitle>{copy.repos.replaceKey}</DialogTitle>
					</DialogHeader>
					<form onSubmit={onVerify} noValidate>
						<FieldGroup>
							<Field>
								<FieldLabel htmlFor="agent-key-id">
									{copy.repos.accessKeyId}
								</FieldLabel>
								<Input
									id="agent-key-id"
									autoComplete="off"
									spellCheck={false}
									disabled={busy}
									value={accessKeyId}
									onChange={(e) => setAccessKeyId(e.target.value)}
								/>
							</Field>
							<Field>
								<FieldLabel htmlFor="agent-key-secret">
									{copy.repos.secretAccessKey}
								</FieldLabel>
								<Input
									id="agent-key-secret"
									type="password"
									autoComplete="off"
									spellCheck={false}
									disabled={busy}
									value={secret}
									onChange={(e) => setSecret(e.target.value)}
								/>
							</Field>
							{error && (
								<Alert variant="destructive">
									<CircleAlert aria-hidden />
									<AlertDescription>{error}</AlertDescription>
								</Alert>
							)}
						</FieldGroup>
						<DialogFooter className="mt-6">
							<Button type="button" variant="secondary" onClick={close}>
								{copy.wizard.keepCurrentKey}
							</Button>
							<Button type="submit" disabled={busy || !accessKeyId || !secret}>
								{verify.isPending && <Spinner />}
								{copy.repos.replaceKeyConfirm}
							</Button>
						</DialogFooter>
					</form>
				</DialogContent>
			</Dialog>

			<AlertDialog open={step === "confirm"}>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>
							{copy.repos.replaceKeyTitle(name)}
						</AlertDialogTitle>
						<AlertDialogDescription>
							{copy.repos.replaceKeyBody(onlineAgents(online))}
							{keyWarningHeading && ` ${keyWarningHeading}.`}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel disabled={busy} onClick={close}>
							{copy.wizard.keepCurrentKey}
						</AlertDialogCancel>
						<AlertDialogAction
							disabled={busy}
							onClick={(event) => {
								event.preventDefault();
								onReplace();
							}}
						>
							{replace.isPending && <Spinner />}
							{copy.repos.replaceKeyConfirm}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</>
	);
}
