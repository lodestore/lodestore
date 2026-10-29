import { useIsMutating, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, CircleAlert, TriangleAlert } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import {
	getCreateTokenMutationKey,
	getListTokensQueryKey,
	useCreateToken,
} from "@/api/gen/endpoints";
import { Scope } from "@/api/gen/model";
import { CodeBlock } from "@/components/code-block";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Dialog,
	DialogContent,
	DialogFooter,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";

const text = copy.tokens;
const SCOPES = Object.values(Scope);
const EXPIRIES = ["30", "90", "365", "never"] as const;
type Expiry = (typeof EXPIRIES)[number];
const COPIED_MS = 2000;
const NAME_MAX = 64;

function FieldMessage({ id, children }: { id: string; children: string }) {
	return (
		<p id={id} className="flex items-start gap-1 text-sm text-destructive">
			<CircleAlert aria-hidden className="mt-1 size-4 shrink-0" />
			{children}
		</p>
	);
}

function TokenSecretView({
	token,
	onDone,
}: {
	token: string;
	onDone: () => void;
}) {
	const [copied, setCopied] = useState(false);
	const timer = useRef<ReturnType<typeof setTimeout>>(undefined);

	useEffect(() => () => clearTimeout(timer.current), []);

	async function onCopy() {
		if (!(await copyText(token))) return;
		setCopied(true);
		clearTimeout(timer.current);
		timer.current = setTimeout(() => setCopied(false), COPIED_MS);
	}

	return (
		<>
			<DialogHeader>
				<DialogTitle className="text-2xl">{text.secret.title}</DialogTitle>
			</DialogHeader>
			<div className="flex flex-col gap-4">
				<Alert role="status">
					<TriangleAlert aria-hidden className="text-warning" />
					<AlertDescription>{text.secret.warning}</AlertDescription>
				</Alert>
				<div data-sensitive>
					<CodeBlock
						text={token}
						object={text.secret.object}
						className="[&_pre]:text-sm"
					/>
				</div>
				<p className="text-sm text-muted-foreground">{text.secret.hint}</p>
			</div>
			<DialogFooter>
				<Button type="button" variant="outline" onClick={onDone}>
					{text.secret.done}
				</Button>
				<Button type="button" onClick={onCopy}>
					{copied ? text.secret.copied : text.secret.copy}
				</Button>
			</DialogFooter>
			<span role="status" className="sr-only">
				{copied ? text.secret.copied : ""}
			</span>
		</>
	);
}

function CreateTokenBody({
	onDone,
	onCreated,
}: {
	onDone: () => void;
	onCreated: (id: string) => void;
}) {
	const id = useId();
	const queryClient = useQueryClient();
	const create = useCreateToken({ mutation: { gcTime: 0 } });
	const [name, setName] = useState("");
	const [picked, setPicked] = useState<ReadonlySet<Scope>>(new Set());
	const [expiry, setExpiry] = useState<Expiry>("90");
	const [submitted, setSubmitted] = useState(false);

	if (create.data) {
		return <TokenSecretView token={create.data.token} onDone={onDone} />;
	}

	const nameError =
		submitted && name.trim() === "" ? text.dialog.nameMissing : undefined;
	const scopesError =
		submitted && picked.size === 0 ? text.dialog.scopesMissing : undefined;

	function toggle(scope: Scope, on: boolean) {
		setPicked((prev) => {
			const next = new Set(prev);
			if (on) next.add(scope);
			else next.delete(scope);
			return next;
		});
	}

	function submit(event: React.FormEvent) {
		event.preventDefault();
		setSubmitted(true);
		if (name.trim() === "" || picked.size === 0 || create.isPending) return;
		create.mutate(
			{
				data: {
					name: name.trim(),
					scopes: SCOPES.filter((scope) => picked.has(scope)),
					expires_in_days: expiry === "never" ? null : Number(expiry),
				},
			},
			{
				onSuccess: ({ item }) => {
					onCreated(item.id);
					void queryClient.invalidateQueries({
						queryKey: getListTokensQueryKey(),
					});
				},
			},
		);
	}

	return (
		<form onSubmit={submit} noValidate className="flex flex-col gap-6">
			<DialogHeader>
				<DialogTitle className="text-2xl">{text.dialog.title}</DialogTitle>
			</DialogHeader>

			<div className="flex flex-col gap-2">
				<Label htmlFor={`${id}-name`}>{text.dialog.name}</Label>
				<Input
					id={`${id}-name`}
					value={name}
					maxLength={NAME_MAX}
					autoComplete="off"
					placeholder={text.dialog.namePlaceholder}
					aria-invalid={nameError !== undefined}
					aria-describedby={nameError ? `${id}-name-error` : undefined}
					className="h-10"
					onChange={(event) => setName(event.target.value)}
				/>
				{nameError && (
					<FieldMessage id={`${id}-name-error`}>{nameError}</FieldMessage>
				)}
			</div>

			<div
				role="group"
				aria-labelledby={`${id}-scopes`}
				aria-describedby={scopesError ? `${id}-scopes-error` : undefined}
				className="flex flex-col gap-2"
			>
				<Label id={`${id}-scopes`}>{text.dialog.permissions}</Label>
				<div className="grid grid-cols-[auto_1fr] gap-x-4">
					{SCOPES.map((scope) => (
						<div
							key={scope}
							className="flex min-h-10 flex-wrap items-center gap-x-2"
						>
							<Checkbox
								id={`${id}-${scope}`}
								checked={picked.has(scope)}
								onCheckedChange={(value) => toggle(scope, value === true)}
							/>
							<Label
								htmlFor={`${id}-${scope}`}
								className="font-mono text-sm font-normal"
							>
								{scope}
							</Label>
							{scope === Scope["metrics:read"] && (
								<span className="text-sm text-muted-foreground">
									{text.dialog.forPrometheus}
								</span>
							)}
							{scope === Scope["snapshots:delete"] && (
								<Badge variant="outline" className="text-warning">
									<TriangleAlert aria-hidden />
									{text.dialog.deletesData}
								</Badge>
							)}
						</div>
					))}
				</div>
				{scopesError && (
					<FieldMessage id={`${id}-scopes-error`}>{scopesError}</FieldMessage>
				)}
				<p className="text-sm text-muted-foreground">
					{text.dialog.scopesHint}
				</p>
			</div>

			<Collapsible className="flex flex-col gap-4">
				<CollapsibleTrigger asChild>
					<Button
						type="button"
						variant="outline"
						size="sm"
						className="group/options h-auto max-w-full self-start py-1"
					>
						<ChevronDown
							aria-hidden
							className="shrink-0 transition-transform group-data-[state=closed]/options:-rotate-90"
						/>
						<span className="font-semibold">{text.dialog.options}</span>
						<span className="min-w-0 truncate font-normal text-muted-foreground">
							{text.dialog.optionsValue(text.expiry[expiry])}
						</span>
					</Button>
				</CollapsibleTrigger>
				<CollapsibleContent className="flex flex-col gap-2">
					<Label htmlFor={`${id}-expires`}>{text.dialog.expires}</Label>
					<Select
						value={expiry}
						onValueChange={(value) => setExpiry(value as Expiry)}
					>
						<SelectTrigger id={`${id}-expires`} className="h-10 w-full">
							<SelectValue />
						</SelectTrigger>
						<SelectContent>
							{EXPIRIES.map((option) => (
								<SelectItem key={option} value={option}>
									{text.expiry[option]}
								</SelectItem>
							))}
						</SelectContent>
					</Select>
					{expiry === "never" && (
						<p className="text-sm text-muted-foreground">
							{text.dialog.noExpiryHint}
						</p>
					)}
				</CollapsibleContent>
			</Collapsible>

			{create.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{copy.errors.submit(text.dialog.createAction, create.error.message)}
					</AlertDescription>
				</Alert>
			)}

			<DialogFooter>
				<Button
					type="button"
					variant="outline"
					disabled={create.isPending}
					onClick={onDone}
				>
					{text.dialog.dismiss}
				</Button>
				<Button type="submit" disabled={create.isPending}>
					{create.isPending && <Spinner />}
					{text.dialog.submit}
				</Button>
			</DialogFooter>
		</form>
	);
}

export function CreateTokenDialog({
	open,
	onOpenChange,
	onFocusRow,
}: {
	open: boolean;
	onOpenChange: (open: boolean) => void;
	onFocusRow: (id: string) => void;
}) {
	const createdId = useRef<string>(undefined);
	const showingSecret = useRef(false);
	const busy = useIsMutating({ mutationKey: getCreateTokenMutationKey() }) > 0;
	return (
		<Dialog
			open={open}
			onOpenChange={(next) => (next || !busy) && onOpenChange(next)}
		>
			<DialogContent
				aria-describedby={undefined}
				className="max-h-[calc(100dvh-2rem)] overflow-y-auto sm:max-w-140"
				onInteractOutside={(event) => {
					if (showingSecret.current) event.preventDefault();
				}}
				onCloseAutoFocus={(event) => {
					showingSecret.current = false;
					const id = createdId.current;
					createdId.current = undefined;
					if (id === undefined) return;
					event.preventDefault();
					onFocusRow(id);
				}}
			>
				<CreateTokenBody
					onDone={() => onOpenChange(false)}
					onCreated={(id) => {
						createdId.current = id;
						showingSecret.current = true;
					}}
				/>
			</DialogContent>
		</Dialog>
	);
}
