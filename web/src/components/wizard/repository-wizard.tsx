import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { checkStorage } from "@/api/gen/endpoints";
import type { RepositorySummary } from "@/api/gen/model";
import { WizardLayout } from "@/components/layout/wizard-layout";
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
import { copy } from "@/copy";
import { repositoriesQuery } from "@/lib/session";
import { AccessStep } from "./access-step";
import { ConnectExistingStep } from "./connect-existing-step";
import {
	BACKENDS,
	type Backend,
	type CheckState,
	type Draft,
	newDraft,
	resetBackend,
	STEPS,
	type Step,
	toApiDraft,
	withoutSecrets,
} from "./draft";
import { FirstHostStep } from "./first-host-step";
import { RecoveryKeyStep } from "./recovery-key-step";
import { StorageStep } from "./storage-step";
import { text } from "./text";

const STORAGE_KEY = "lodestore.wizard.draft";
const PERSISTED = [
	"name",
	"backend",
	"endpoint",
	"region",
	"bucket",
	"prefix",
	"pathStyle",
	"path",
] as const satisfies readonly (keyof Draft)[];

export function loadDraft(): Draft {
	const draft = newDraft();
	try {
		const stored = JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? "null");
		if (stored === null || typeof stored !== "object") return draft;
		const backend = BACKENDS.find((b) => b === stored.backend);
		if (!backend) return draft;
		draft.backend = backend;
		for (const key of [
			"name",
			"endpoint",
			"region",
			"bucket",
			"prefix",
			"path",
		] as const) {
			if (typeof stored[key] === "string") draft[key] = stored[key];
		}
		if (typeof stored.pathStyle === "boolean")
			draft.pathStyle = stored.pathStyle;
	} catch {
		return newDraft();
	}
	return draft;
}

export function saveDraft(draft: Draft) {
	const persisted: Record<string, unknown> = {};
	for (const key of PERSISTED) persisted[key] = draft[key];
	try {
		sessionStorage.setItem(STORAGE_KEY, JSON.stringify(persisted));
	} catch {
		return;
	}
}

export function clearSavedDraft() {
	try {
		sessionStorage.removeItem(STORAGE_KEY);
	} catch {
		return;
	}
}

export type WizardMode = "welcome" | "settings";

const CHECK_TIMEOUT_MS = 60_000;

export function RepositoryWizard({
	mode,
	step,
}: {
	mode: WizardMode;
	step: Step | undefined;
}) {
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const [draft, setDraft] = useState(loadDraft);
	const [check, setCheck] = useState<CheckState>({ phase: "idle" });
	const [created, setCreated] = useState<RepositorySummary>();
	const [rebuilding, setRebuilding] = useState(false);
	const [confirmExit, setConfirmExit] = useState(false);

	const steps = mode === "welcome" ? STEPS : STEPS.slice(0, 3);
	const current: Step = step ?? "storage";
	const checked =
		check.phase === "done" && !check.stale && !check.result.blocking;

	function go(next: Step, replace = false) {
		if (mode === "welcome") {
			void navigate({ to: "/welcome", search: { step: next }, replace });
		} else {
			void navigate({
				to: "/settings/repositories/new",
				search: { step: next },
				replace,
			});
		}
	}

	useEffect(() => saveDraft(draft), [draft]);

	const first = useRef(true);
	useEffect(() => {
		const fresh = first.current;
		first.current = false;
		const allowed =
			current === "storage" ||
			(current === "access" && !fresh) ||
			(current === "key" && checked) ||
			(current === "host" && created !== undefined && mode === "welcome");
		if (!allowed) go("storage", true);
	});

	function change(patch: Partial<Draft>) {
		setDraft((d) => ({ ...d, ...patch }));
		setCheck((c) => (c.phase === "done" ? { ...c, stale: true } : c));
	}

	function chooseBackend(backend: Backend) {
		if (backend === draft.backend) return;
		setDraft((d) => resetBackend(d, backend));
		setCheck({ phase: "idle" });
	}

	async function runCheck() {
		setCheck({ phase: "running" });
		try {
			const result = await checkStorage(toApiDraft(draft), {
				signal: AbortSignal.timeout(CHECK_TIMEOUT_MS),
			});
			setCheck({ phase: "done", result, stale: false });
		} catch (error) {
			const message = (error instanceof Error ? error.message : "").replace(
				/\.$/,
				"",
			);
			setCheck({
				phase: "error",
				message: copy.errors.submit(text.checkAction, message || undefined),
			});
		}
	}

	async function finish(
		repository: RepositorySummary,
		message: string,
		rebuilding = false,
	) {
		clearSavedDraft();
		setDraft((d) => withoutSecrets(d));
		setCreated(repository);
		setRebuilding(rebuilding);
		toast.success(
			message,
			rebuilding
				? {
						description: copy.catalog.connectedExisting,
						action: {
							label: copy.catalog.openRestore,
							onClick: () => void navigate({ to: "/restore" }),
						},
					}
				: undefined,
		);
		await queryClient.invalidateQueries({
			queryKey: repositoriesQuery.queryKey,
		});
		if (mode === "welcome") {
			go("host");
		} else {
			void navigate({
				to: "/settings/repositories/$id",
				params: { id: repository.id },
			});
		}
	}

	function leave() {
		clearSavedDraft();
		void navigate({ to: "/settings/repositories" });
	}

	function exit() {
		if (JSON.stringify(draft) === JSON.stringify(newDraft())) leave();
		else setConfirmExit(true);
	}

	const wide =
		(current === "access" && draft.backend === "s3") || current === "host";
	const stepInfo = steps.map((id) => {
		const i = STEPS.indexOf(id);
		return {
			title: copy.wizard.steps[i],
			description: copy.wizard.stepDescriptions[i],
		};
	});

	return (
		<WizardLayout
			mode={mode}
			steps={stepInfo}
			current={Math.max(0, steps.indexOf(current))}
			wide={wide}
		>
			<div className="flex flex-1 flex-col gap-8">
				{mode === "welcome" ? (
					<header className="flex flex-col gap-2">
						<h1
							data-page-title
							className="text-2xl font-semibold outline-none lg:text-display"
						>
							{copy.wizard.title}
						</h1>
						<p className="text-sm text-muted-foreground">
							{copy.wizard.subtitle}
						</p>
					</header>
				) : (
					<header className="flex items-start justify-between gap-4">
						<h1 data-page-title className="text-2xl font-semibold outline-none">
							{copy.wizard.newTitle}
						</h1>
						<Button type="button" variant="outline" onClick={exit}>
							{copy.wizard.exit}
						</Button>
					</header>
				)}
				{current === "storage" && (
					<StorageStep
						draft={draft}
						onChange={change}
						onBackend={chooseBackend}
						onContinue={() => go("access")}
					/>
				)}
				{current === "access" && (
					<AccessStep
						draft={draft}
						onChange={change}
						check={check}
						onCheck={runCheck}
						onBack={() => go("storage")}
						onContinue={() => go("key")}
					/>
				)}
				{current === "key" &&
					checked &&
					(check.result.existing_repository ? (
						<ConnectExistingStep
							draft={draft}
							onBack={() => go("access")}
							onConnected={(repository) =>
								void finish(repository, copy.existingRepo.connected, true)
							}
						/>
					) : (
						<RecoveryKeyStep
							draft={draft}
							onBack={() => go("access")}
							onCreated={(repository) =>
								void finish(
									repository,
									copy.recoveryKey.createdNamed(repository.name),
								)
							}
						/>
					))}
				{current === "host" && created && (
					<FirstHostStep repositoryId={created.id} rebuilding={rebuilding} />
				)}
			</div>
			<AlertDialog open={confirmExit} onOpenChange={setConfirmExit}>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>{copy.wizard.discard.title}</AlertDialogTitle>
						<AlertDialogDescription>
							{copy.wizard.discard.body}
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel>{copy.wizard.discard.keep}</AlertDialogCancel>
						<AlertDialogAction onClick={leave}>
							{copy.wizard.discard.leave}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</WizardLayout>
	);
}
