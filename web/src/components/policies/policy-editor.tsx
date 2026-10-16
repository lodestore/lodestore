import { useQueryClient } from "@tanstack/react-query";
import { useBlocker, useNavigate } from "@tanstack/react-router";
import {
	type ReactNode,
	type RefObject,
	useEffect,
	useRef,
	useState,
} from "react";
import { toast } from "sonner";
import {
	getGetPolicyQueryKey,
	getListPoliciesQueryKey,
	useCreatePolicy,
	useUpdatePolicy,
	useValidatePolicy,
} from "@/api/gen/endpoints";
import type {
	PolicyDetail,
	PolicyInput,
	RepositorySummary,
	ValidateResponse,
} from "@/api/gen/model";
import { MatchingHosts } from "@/components/policies/matching-hosts";
import {
	draftFromPolicy,
	type PolicyDraft,
	PolicyForm,
	policyFromDraft,
	problemsByPath,
} from "@/components/policies/policy-form";
import { PolicyYamlEditor } from "@/components/policies/policy-yaml";
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
import { Spinner } from "@/components/ui/spinner";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";

const DEBOUNCE_MS = 400;

type Tab = "form" | "yaml";
type Input = Pick<PolicyInput, "policy" | "yaml">;

function initialState(
	detail: PolicyDetail | undefined,
	starter: PolicyDraft | undefined,
) {
	if (detail) {
		const draft = draftFromPolicy(detail.policy);
		const result: ValidateResponse = {
			valid: true,
			errors: [],
			name_taken: false,
			policy: detail.policy,
			yaml: detail.yaml,
			matching: detail.matching,
			warnings: detail.warnings,
		};
		return {
			draft,
			yaml: detail.yaml,
			json: JSON.stringify(policyFromDraft(draft)),
			result,
		};
	}
	const draft = starter as PolicyDraft;
	return {
		draft,
		yaml: "",
		json: JSON.stringify(policyFromDraft(draft)),
		result: undefined,
	};
}

export function PolicyEditor({
	detail,
	starter,
	repositories,
	onChangedChange,
	leaving,
	children,
}: {
	detail?: PolicyDetail;
	starter?: PolicyDraft;
	repositories: RepositorySummary[];
	onChangedChange?: (changed: boolean) => void;
	leaving?: RefObject<boolean>;
	children?: ReactNode;
}) {
	const isNew = detail === undefined;
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const [initial, setInitial] = useState(() => initialState(detail, starter));
	const [baseRevision, setBaseRevision] = useState(detail?.revision);
	const [draft, setDraft] = useState<PolicyDraft>(initial.draft);
	const [yaml, setYaml] = useState(initial.yaml);
	const [source, setSource] = useState<Tab>("form");
	const [tab, setTab] = useState<Tab>("form");
	const [result, setResult] = useState(initial.result);
	const [settled, setSettled] = useState(!isNew);
	const [blocked, setBlocked] = useState(false);
	const [formRefused, setFormRefused] = useState(false);
	const [checkFailed, setCheckFailed] = useState(false);
	const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
	const latest = useRef(0);
	const ownBypass = useRef(false);
	const bypass = leaving ?? ownBypass;
	const yamlField = useRef<HTMLTextAreaElement>(null);
	const validate = useValidatePolicy();
	const create = useCreatePolicy();
	const update = useUpdatePolicy();
	const saving = create.isPending || update.isPending;
	const conflict = update.error?.code === "policy_conflict";
	const saveError = conflict ? null : (create.error ?? update.error);

	function check(input: Input, after?: (response: ValidateResponse) => void) {
		const ticket = ++latest.current;
		setCheckFailed(false);
		validate.mutate(
			{ data: { ...input, original_name: detail?.name } },
			{
				onSuccess: (response) => {
					if (ticket !== latest.current) return;
					setResult(response);
					setSettled(true);
					if (response.valid && input.yaml != null && response.policy) {
						setDraft(draftFromPolicy(response.policy));
					} else if (response.valid && response.yaml != null) {
						setYaml(response.yaml);
					}
					after?.(response);
				},
				onError: () => {
					if (ticket === latest.current) setCheckFailed(true);
				},
			},
		);
	}

	function schedule(input: Input) {
		clearTimeout(timer.current);
		latest.current += 1;
		setSettled(false);
		timer.current = setTimeout(() => check(input), DEBOUNCE_MS);
	}

	useEffect(() => {
		if (isNew) check({ policy: policyFromDraft(initial.draft) });
		return () => {
			clearTimeout(timer.current);
			latest.current += 1;
		};
	}, []);

	function editForm(next: PolicyDraft) {
		setDraft(next);
		setFormRefused(false);
		setSource("form");
		schedule({ policy: policyFromDraft(next) });
	}

	function editYaml(next: string) {
		setYaml(next);
		setSource("yaml");
		setBlocked(false);
		schedule({ yaml: next });
	}

	function refuseForm() {
		setBlocked(true);
		setTimeout(() => yamlField.current?.focus(), 0);
	}

	function changeTab(next: string) {
		if (next === "form") {
			if (source === "form") return setTab("form");
			if (settled) return result?.valid ? setTab("form") : refuseForm();
			clearTimeout(timer.current);
			return check({ yaml }, (response) =>
				response.valid ? setTab("form") : refuseForm(),
			);
		}
		if (source === "yaml") return setTab("yaml");
		const showYaml = (response: ValidateResponse) =>
			response.valid ? setTab("yaml") : setFormRefused(true);
		if (settled && result) return showYaml(result);
		clearTimeout(timer.current);
		check({ policy: policyFromDraft(draft) }, showYaml);
	}

	const changed =
		source === "yaml"
			? isNew || yaml !== initial.yaml
			: JSON.stringify(policyFromDraft(draft)) !== initial.json;
	useEffect(() => {
		onChangedChange?.(changed);
	}, [changed, onChangedChange]);

	function rebase(next: PolicyDetail) {
		const fresh = initialState(next, undefined);
		clearTimeout(timer.current);
		latest.current += 1;
		create.reset();
		update.reset();
		setInitial(fresh);
		setBaseRevision(next.revision);
		setDraft(fresh.draft);
		setYaml(fresh.yaml);
		setSource("form");
		setBlocked(false);
		setResult(fresh.result);
		setSettled(true);
	}

	const behind = detail !== undefined && detail.revision > (baseRevision ?? 0);
	useEffect(() => {
		if (detail && behind && !changed) rebase(detail);
	}, [behind, changed]);

	const nameTaken = isNew && result?.name_taken === true;
	const noHostsPicked =
		source === "form" && draft.hostMode === "list" && draft.hosts.length === 0;
	const canSave =
		!noHostsPicked &&
		(isNew || changed) &&
		settled &&
		result?.valid === true &&
		!nameTaken &&
		!behind &&
		!saving;

	function save() {
		const data: PolicyInput =
			source === "yaml" ? { yaml } : { policy: policyFromDraft(draft) };
		const done = (saved: PolicyDetail) => {
			toast.success(copy.policies.editor.saved(saved.name));
			queryClient.setQueryData(getGetPolicyQueryKey(saved.name), saved);
			void queryClient.invalidateQueries({
				queryKey: getListPoliciesQueryKey(),
			});
			if (isNew) {
				bypass.current = true;
				void navigate({ to: "/policies/$name", params: { name: saved.name } });
			} else {
				rebase(saved);
			}
		};
		if (detail) {
			update.mutate(
				{
					name: detail.name,
					data: { ...data, base_revision: baseRevision ?? 0 },
				},
				{
					onSuccess: done,
					onError: (error) => {
						if (error.code === "policy_conflict") {
							void queryClient.invalidateQueries({
								queryKey: getGetPolicyQueryKey(detail.name),
							});
						}
					},
				},
			);
		} else create.mutate({ data }, { onSuccess: done });
	}

	function discard() {
		clearTimeout(timer.current);
		latest.current += 1;
		create.reset();
		update.reset();
		setDraft(initial.draft);
		setYaml(initial.yaml);
		setSource("form");
		setBlocked(false);
		setResult(initial.result);
		setSettled(!isNew);
		if (isNew) check({ policy: policyFromDraft(initial.draft) });
	}

	const blocker = useBlocker({
		shouldBlockFn: () => changed && !bypass.current,
		enableBeforeUnload: () => changed && !bypass.current,
		withResolver: true,
	});

	const text = copy.policies;
	const failure = settled && result && !result.valid ? result : undefined;
	const yamlProblems = source === "yaml" ? (failure?.errors ?? []) : [];
	const hasLabels = draft.labels.some((label) => label.key !== "");

	return (
		<div className="grid gap-6 xl:grid-cols-[minmax(0,1fr)_360px]">
			<div className="flex min-w-0 flex-col gap-6 xl:col-start-1 xl:row-start-1">
				{(behind || conflict) && changed && detail && (
					<Alert>
						<AlertDescription className="flex items-center justify-between gap-4">
							{text.editor.changedElsewhere}
							<Button
								type="button"
								variant="secondary"
								size="sm"
								disabled={!behind}
								onClick={() => rebase(detail)}
							>
								{text.editor.reload}
							</Button>
						</AlertDescription>
					</Alert>
				)}
				<Tabs value={tab} onValueChange={changeTab}>
					<TabsList variant="line">
						<TabsTrigger value="form" className="after:bg-primary-strong">
							{text.editor.tabForm}
						</TabsTrigger>
						<TabsTrigger value="yaml" className="after:bg-primary-strong">
							{text.editor.tabYaml}
						</TabsTrigger>
					</TabsList>
					<TabsContent value="form" className="pt-4">
						<PolicyForm
							draft={draft}
							onChange={editForm}
							problems={problemsByPath(failure?.errors ?? [])}
							repositories={repositories}
							nameLocked={!isNew}
							nameTaken={nameTaken}
						/>
					</TabsContent>
					<TabsContent value="yaml" className="pt-4">
						<PolicyYamlEditor
							value={yaml}
							onChange={editYaml}
							problems={yamlProblems}
							blocked={blocked}
							textareaRef={yamlField}
						/>
					</TabsContent>
				</Tabs>

				{formRefused && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>{text.editor.formBlocked}</AlertDescription>
					</Alert>
				)}
				{saveError && (
					<Alert variant="destructive" role="alert">
						<AlertDescription>
							{text.editor.saveFailed(saveError.message)}
						</AlertDescription>
					</Alert>
				)}
				{failure && source === "form" && (isNew || changed) && (
					<p className="text-sm text-muted-foreground">
						{text.editor.cannotSave(failure.errors[0]?.message ?? "")}
					</p>
				)}
				<div className="flex gap-2">
					<Button
						type="button"
						variant={isNew || changed ? "default" : "outline"}
						disabled={!canSave}
						onClick={save}
					>
						{saving && <Spinner />}
						{text.save}
					</Button>
					{changed && (
						<Button
							type="button"
							variant="outline"
							disabled={saving}
							onClick={discard}
						>
							{text.discard}
						</Button>
					)}
				</div>
			</div>

			<div
				className={cn(
					"grid min-w-0 items-start gap-6 xl:col-start-2 xl:row-start-1 xl:self-start",
					children ? "md:grid-cols-2 xl:grid-cols-1" : "xl:sticky xl:top-6",
				)}
			>
				<MatchingHosts
					matching={result?.valid ? (result.matching ?? undefined) : undefined}
					warnings={result?.valid ? result.warnings : []}
					selectorEmpty={draft.hostMode === "labels" && !hasLabels}
					notice={noHostsPicked ? text.form.pickAtLeastOne : undefined}
					invalid={failure !== undefined}
					refreshing={!settled && !checkFailed}
					failed={checkFailed}
					onRetry={() =>
						check(
							source === "yaml" ? { yaml } : { policy: policyFromDraft(draft) },
						)
					}
				/>
				{children}
			</div>

			<AlertDialog open={blocker.status === "blocked"}>
				<AlertDialogContent>
					<AlertDialogHeader>
						<AlertDialogTitle>{text.leave.title}</AlertDialogTitle>
						<AlertDialogDescription>{text.leave.body}</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel onClick={() => blocker.reset?.()}>
							{text.leave.keep}
						</AlertDialogCancel>
						<AlertDialogAction
							variant="destructive"
							onClick={() => blocker.proceed?.()}
						>
							{text.leave.leave}
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</div>
	);
}
