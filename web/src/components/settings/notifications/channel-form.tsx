import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { CircleCheck, CircleX, Lock } from "lucide-react";
import {
	type FormEvent,
	type ReactNode,
	useEffect,
	useRef,
	useState,
} from "react";
import { toast } from "sonner";
import {
	getGetChannelQueryKey,
	getListChannelsQueryKey,
	useCreateChannel,
	useGetAccount,
	useTestChannel,
	useUpdateChannel,
} from "@/api/gen/endpoints";
import {
	AlertType,
	type ChannelDto,
	type ChannelInput,
	ChannelKind,
	type ChannelSettingsDto,
	type FieldError,
	SmtpMode,
} from "@/api/gen/model";
import { Fact } from "@/components/fact";
import { TextWithPaths } from "@/components/path-text";
import { ErrorText, RowField } from "@/components/policies/form-field";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import {
	Breadcrumb,
	BreadcrumbItem,
	BreadcrumbLink,
	BreadcrumbList,
	BreadcrumbPage,
	BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";
import { Button, buttonVariants } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
	Field,
	FieldContent,
	FieldGroup,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { copy } from "@/copy";
import { cn } from "@/lib/utils";
import { KindLabel } from "./channels-table";

const text = copy.notifications;
const form = text.form;
const TOAST_MS = 4000;
const ALERT_TYPES = Object.values(AlertType);
const KINDS = Object.values(ChannelKind);

type Draft = {
	kind: ChannelKind;
	name: string;
	secret: string;
	url: string;
	chatId: string;
	recipients: string;
	smtpHost: string;
	port: string;
	security: SmtpMode;
	username: string;
	from: string;
	alerts: ReadonlySet<AlertType>;
	enabled: boolean;
};

type Errors = ReadonlyMap<string, string>;

const blank = (kind: ChannelKind): Draft => ({
	kind,
	name: "",
	secret: "",
	url: "",
	chatId: "",
	recipients: "",
	smtpHost: "",
	port: "587",
	security: SmtpMode.starttls,
	username: "",
	from: "",
	alerts: new Set(ALERT_TYPES),
	enabled: true,
});

const fromChannel = (channel: ChannelDto): Draft => {
	const { settings } = channel;
	return {
		...blank(channel.type),
		name: channel.name,
		url: settings.url ?? "",
		chatId: settings.chat_id ?? "",
		recipients: (settings.recipients ?? []).join(", "),
		smtpHost: settings.smtp_host ?? "",
		port: String(settings.port ?? 587),
		security: settings.security ?? SmtpMode.starttls,
		username: settings.username ?? "",
		from: settings.from ?? "",
		alerts: new Set(channel.alert_types),
		enabled: channel.enabled,
	};
};

const recipientList = (value: string) =>
	value
		.split(/[,\s]+/)
		.map((entry) => entry.trim())
		.filter(Boolean);

function settingsOf(draft: Draft): ChannelSettingsDto {
	switch (draft.kind) {
		case ChannelKind.telegram:
			return { chat_id: draft.chatId.trim() };
		case ChannelKind.slack:
			return {};
		case ChannelKind.email:
			return {
				recipients: recipientList(draft.recipients),
				smtp_host: draft.smtpHost.trim(),
				port: Number(draft.port),
				security: draft.security,
				username: draft.username.trim() || null,
				from: draft.from.trim() || null,
			};
		default:
			return { url: draft.url.trim() };
	}
}

function toInput(draft: Draft): ChannelInput {
	const secret = draft.secret.trim();
	return {
		type: draft.kind,
		name: draft.name.trim(),
		enabled: draft.enabled,
		alert_types: ALERT_TYPES.filter((type) => draft.alerts.has(type)),
		settings: settingsOf(draft),
		secret: secret === "" ? null : secret,
	};
}

function validate(draft: Draft, secretSaved: boolean, adminEmail?: string) {
	const errors = new Map<string, string>();
	const invalid = text.validation;
	const secret = draft.secret.trim();
	const missingSecret = secret === "" && !secretSaved;
	if (draft.name.trim() === "") errors.set("name", invalid.name);
	switch (draft.kind) {
		case ChannelKind.telegram:
			if (missingSecret) errors.set("secret", invalid.botToken);
			if (draft.chatId.trim() === "") errors.set("chat_id", invalid.chatId);
			break;
		case ChannelKind.slack:
			if (secret === "" ? !secretSaved : !secret.startsWith("https://")) {
				errors.set("secret", invalid.slackUrl);
			}
			break;
		case ChannelKind.email: {
			if (recipientList(draft.recipients).length === 0 && !adminEmail) {
				errors.set("recipients", invalid.recipients);
			}
			if (draft.smtpHost.trim() === "")
				errors.set("smtp_host", invalid.smtpHost);
			const port = Number(draft.port);
			if (!/^\d+$/.test(draft.port.trim()) || port < 1 || port > 65535) {
				errors.set("port", invalid.port);
			}
			break;
		}
		default:
			if (!/^https?:\/\/\S+/.test(draft.url.trim())) {
				errors.set("url", invalid.webhookUrl);
			}
	}
	if (draft.alerts.size === 0) errors.set("alert_types", invalid.alertTypes);
	return errors;
}

function destinationOf(draft: Draft, adminEmail?: string) {
	const destinations = text.test.destinations;
	switch (draft.kind) {
		case ChannelKind.telegram:
			return destinations.telegram(draft.chatId.trim());
		case ChannelKind.slack:
			return destinations.slack;
		case ChannelKind.email:
			return destinations.email(
				recipientList(draft.recipients).join(", ") || adminEmail || "",
			);
		default:
			return destinations.webhook;
	}
}

function serverErrors(errors: FieldError[] | undefined, kind: ChannelKind) {
	const out = new Map<string, string>();
	for (const { path, message } of errors ?? []) {
		const key =
			kind === ChannelKind.slack && path === "settings.url"
				? "secret"
				: path.replace(/^settings\./, "");
		if (!out.has(key)) out.set(key, message);
	}
	return out;
}

export function ChannelSecretField({
	label,
	secretName,
	saved,
	value,
	onChange,
	error,
	helper,
}: {
	label: string;
	secretName: string;
	saved: boolean;
	value: string;
	onChange: (value: string) => void;
	error?: string;
	helper?: string;
}) {
	const [replacing, setReplacing] = useState(false);
	const input = useRef<HTMLInputElement>(null);

	useEffect(() => {
		if (replacing) input.current?.focus();
	}, [replacing]);

	const showSaved = saved && !replacing;
	return (
		<RowField label={label} error={error} helper={helper}>
			{(a11y) => (
				<div className="flex flex-wrap items-center gap-2">
					{showSaved ? (
						<div
							id={a11y.id}
							className="flex h-10 min-w-0 flex-1 items-center gap-2 rounded-md bg-muted px-4"
						>
							<span aria-hidden className="font-mono">
								{text.secret.mask}
							</span>
							<span aria-hidden className="text-sm text-muted-foreground">
								{text.secret.saved}
							</span>
							<span className="sr-only">{text.secret.savedAria(label)}</span>
						</div>
					) : (
						<Input
							{...a11y}
							ref={input}
							name="channel-secret"
							type="password"
							autoComplete="new-password"
							data-1p-ignore
							data-lpignore="true"
							autoCapitalize="off"
							spellCheck={false}
							className="min-w-0 flex-1 font-mono"
							value={value}
							onChange={(event) => onChange(event.target.value)}
						/>
					)}
					{saved && (
						<Button
							type="button"
							variant="outline"
							onClick={() => {
								if (replacing) onChange("");
								setReplacing(!replacing);
							}}
						>
							{replacing ? text.secret.keep : text.secret.replace(secretName)}
						</Button>
					)}
				</div>
			)}
		</RowField>
	);
}

export function AlertTypeChecklist({
	selected,
	onChange,
	error,
}: {
	selected: ReadonlySet<AlertType>;
	onChange: (next: ReadonlySet<AlertType>) => void;
	error?: string;
}) {
	const firstInvalid = error ? ALERT_TYPES[0] : undefined;
	return (
		<FieldSet aria-describedby={error ? "alert-types-error" : undefined}>
			<FieldLegend className="mb-0">{form.sendAlerts}</FieldLegend>
			<div className="flex flex-col">
				{ALERT_TYPES.map((type) => {
					const id = `alert-type-${type}`;
					return (
						<div key={type} className="flex min-h-10 items-center gap-2">
							<Checkbox
								id={id}
								checked={selected.has(type)}
								aria-invalid={type === firstInvalid ? true : undefined}
								onCheckedChange={(on) => {
									const next = new Set(selected);
									if (on === true) next.add(type);
									else next.delete(type);
									onChange(next);
								}}
							/>
							<FieldLabel htmlFor={id} className="font-normal">
								{text.alertTypes[type]}
							</FieldLabel>
						</div>
					);
				})}
			</div>
			<ErrorText id="alert-types-error" message={error} />
		</FieldSet>
	);
}

export type TestOutcome = { ok: boolean; message: string };

export function SendTestResult({ outcome }: { outcome?: TestOutcome }) {
	if (!outcome) return null;
	return (
		<Alert
			role="status"
			variant={outcome.ok ? "default" : "destructive"}
			className="w-full wrap-anywhere"
		>
			{outcome.ok ? (
				<CircleCheck aria-hidden className="text-success" />
			) : (
				<CircleX aria-hidden />
			)}
			<AlertDescription>
				<TextWithPaths text={outcome.message} />
			</AlertDescription>
		</Alert>
	);
}

export function ChannelPageShell({
	crumb,
	title,
	children,
}: {
	crumb: string;
	title: string;
	children: ReactNode;
}) {
	return (
		<div className="flex max-w-[720px] flex-col gap-6 pb-8">
			<Breadcrumb>
				<BreadcrumbList>
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/settings">{text.settings}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbLink asChild>
							<Link to="/settings/notifications">{text.title}</Link>
						</BreadcrumbLink>
					</BreadcrumbItem>
					<BreadcrumbSeparator />
					<BreadcrumbItem>
						<BreadcrumbPage>{crumb}</BreadcrumbPage>
					</BreadcrumbItem>
				</BreadcrumbList>
			</Breadcrumb>
			<h1 data-page-title className="text-2xl font-semibold wrap-anywhere">
				{title}
			</h1>
			{children}
		</div>
	);
}

export function BackToNotifications() {
	return (
		<Link
			to="/settings/notifications"
			className={buttonVariants({ variant: "outline" })}
		>
			{text.back}
		</Link>
	);
}

function TextField({
	label,
	field,
	value,
	onChange,
	errors,
	helper,
	mono = false,
	inputMode,
}: {
	label: string;
	field: string;
	value: string;
	onChange: (value: string) => void;
	errors: Errors;
	helper?: string;
	mono?: boolean;
	inputMode?: "url" | "numeric";
}) {
	return (
		<RowField label={label} helper={helper} error={errors.get(field)}>
			{(a11y) => (
				<Input
					{...a11y}
					name={field}
					inputMode={inputMode}
					autoComplete="off"
					autoCapitalize="off"
					spellCheck={false}
					className={cn("h-10", mono && "font-mono")}
					value={value}
					onChange={(event) => onChange(event.target.value)}
				/>
			)}
		</RowField>
	);
}

function KindFields({
	draft,
	errors,
	patch,
	secretSaved,
	adminEmail,
}: {
	draft: Draft;
	errors: Errors;
	patch: (next: Partial<Draft>) => void;
	secretSaved: boolean;
	adminEmail?: string;
}) {
	const secret = (label: string, secretName: string, helper?: string) => (
		<ChannelSecretField
			label={label}
			secretName={secretName}
			saved={secretSaved}
			value={draft.secret}
			onChange={(value) => patch({ secret: value })}
			error={errors.get("secret")}
			helper={helper}
		/>
	);
	switch (draft.kind) {
		case ChannelKind.telegram:
			return (
				<>
					{secret(form.botToken, form.botTokenName)}
					<TextField
						label={form.chatId}
						field="chat_id"
						value={draft.chatId}
						onChange={(chatId) => patch({ chatId })}
						errors={errors}
						helper={form.chatIdHint}
						mono
					/>
				</>
			);
		case ChannelKind.slack:
			return secret(form.slackUrl, form.slackUrlName, form.slackHint);
		case ChannelKind.email:
			return (
				<>
					<TextField
						label={form.recipients}
						field="recipients"
						value={draft.recipients}
						onChange={(recipients) => patch({ recipients })}
						errors={errors}
						helper={adminEmail ? form.recipientsHint(adminEmail) : undefined}
					/>
					<TextField
						label={form.smtpHost}
						field="smtp_host"
						value={draft.smtpHost}
						onChange={(smtpHost) => patch({ smtpHost })}
						errors={errors}
						mono
					/>
					<div className="grid grid-cols-1 gap-6 @min-[480px]:grid-cols-2">
						<TextField
							label={form.port}
							field="port"
							value={draft.port}
							onChange={(port) => patch({ port })}
							errors={errors}
							inputMode="numeric"
							mono
						/>
						<RowField label={form.security} error={errors.get("security")}>
							{(a11y) => (
								<Select
									value={draft.security}
									onValueChange={(value) =>
										patch({ security: value as SmtpMode })
									}
								>
									<SelectTrigger {...a11y} className="h-10 w-full">
										<SelectValue />
									</SelectTrigger>
									<SelectContent>
										<SelectItem value={SmtpMode.starttls}>
											{form.starttls}
										</SelectItem>
										<SelectItem value={SmtpMode.tls}>{form.tls}</SelectItem>
									</SelectContent>
								</Select>
							)}
						</RowField>
					</div>
					<TextField
						label={form.username}
						field="username"
						value={draft.username}
						onChange={(username) => patch({ username })}
						errors={errors}
					/>
					{secret(form.password, form.passwordName)}
					<TextField
						label={form.from}
						field="from"
						value={draft.from}
						onChange={(from) => patch({ from })}
						errors={errors}
					/>
				</>
			);
		default:
			return (
				<>
					<TextField
						label={form.url}
						field="url"
						value={draft.url}
						onChange={(url) => patch({ url })}
						errors={errors}
						helper={form.webhookHint}
						inputMode="url"
						mono
					/>
					{secret(form.secret, form.secretName)}
				</>
			);
	}
}

export function ChannelForm({ channel }: { channel?: ChannelDto }) {
	const editing = channel !== undefined;
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const account = useGetAccount();
	const create = useCreateChannel();
	const update = useUpdateChannel();
	const test = useTestChannel();
	const root = useRef<HTMLFormElement>(null);
	const [draft, setDraft] = useState<Draft>(() =>
		channel ? fromChannel(channel) : blank(ChannelKind.webhook),
	);
	const [submitted, setSubmitted] = useState(false);
	const [server, setServer] = useState<Errors>(new Map());
	const [focusTick, setFocusTick] = useState(0);
	const [testedDestination, setTestedDestination] = useState("");
	const saving = editing ? update : create;
	const adminEmail = account.data?.email ?? undefined;

	useEffect(() => {
		if (focusTick > 0) {
			root.current
				?.querySelector<HTMLElement>('[aria-invalid="true"]')
				?.focus();
		}
	}, [focusTick]);

	const secretSaved = editing && channel.secret_set;
	const check = () => validate(draft, secretSaved, adminEmail);
	const errors = new Map<string, string>(submitted ? check() : []);
	for (const [key, message] of server)
		if (!errors.has(key)) errors.set(key, message);

	function patch(next: Partial<Draft>) {
		setDraft((prev) => ({ ...prev, ...next }));
		setServer(new Map());
		if (!test.isPending) test.reset();
	}

	function showServerErrors(list: FieldError[] | undefined) {
		setServer(serverErrors(list, draft.kind));
		setFocusTick((n) => n + 1);
	}

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (saving.isPending) return;
		setSubmitted(true);
		if (check().size > 0) {
			setFocusTick((n) => n + 1);
			return;
		}
		const done = {
			onSuccess: () => {
				toast.success(form.saved, { duration: TOAST_MS });
				void queryClient.invalidateQueries({
					queryKey: getListChannelsQueryKey(),
				});
				if (channel) {
					void queryClient.invalidateQueries({
						queryKey: getGetChannelQueryKey(channel.id),
					});
				}
				void navigate({ to: "/settings/notifications" });
			},
			onError: (error: { status: number; errors?: FieldError[] }) => {
				if (error.status === 422) showServerErrors(error.errors);
			},
		};
		const data = toInput(draft);
		if (channel) update.mutate({ id: channel.id, data }, done);
		else create.mutate({ data }, done);
	}

	function sendTest() {
		if (test.isPending) return;
		setSubmitted(true);
		if (check().size > 0) {
			setFocusTick((n) => n + 1);
			return;
		}
		setTestedDestination(destinationOf(draft, adminEmail));
		test.mutate(
			{
				data: {
					channel_id: channel?.id ?? null,
					channel: toInput(draft),
				},
			},
			{
				onError: (error) => {
					if (error.status === 422) showServerErrors(error.errors);
				},
			},
		);
	}

	const outcome: TestOutcome | undefined = test.data
		? test.data.ok
			? { ok: true, message: text.test.sent(testedDestination) }
			: { ok: false, message: text.test.failed(test.data.message) }
		: test.isError && test.error.status !== 422
			? { ok: false, message: text.test.failed(test.error.message) }
			: undefined;

	return (
		<form
			ref={root}
			noValidate
			onSubmit={onSubmit}
			className="flex flex-col gap-6"
		>
			<FieldGroup className="gap-6">
				{editing ? (
					<dl>
						<Fact label={form.type}>
							<KindLabel kind={draft.kind} />
						</Fact>
					</dl>
				) : (
					<RowField label={form.type}>
						{(a11y) => (
							<Select
								value={draft.kind}
								onValueChange={(value) => {
									const kind = value as ChannelKind;
									setDraft((prev) => ({
										...blank(kind),
										name: prev.name,
										alerts: prev.alerts,
										enabled: prev.enabled,
									}));
									setServer(new Map());
									test.reset();
								}}
							>
								<SelectTrigger {...a11y} className="h-10 w-full">
									<SelectValue />
								</SelectTrigger>
								<SelectContent>
									{KINDS.map((kind) => (
										<SelectItem key={kind} value={kind}>
											{text.kinds[kind]}
										</SelectItem>
									))}
								</SelectContent>
							</Select>
						)}
					</RowField>
				)}

				<RowField label={form.name} error={errors.get("name")}>
					{(a11y) => (
						<Input
							{...a11y}
							name="name"
							autoComplete="off"
							placeholder={form.namePlaceholder}
							className="h-10"
							value={draft.name}
							onChange={(event) => patch({ name: event.target.value })}
						/>
					)}
				</RowField>

				<KindFields
					draft={draft}
					errors={errors}
					patch={patch}
					secretSaved={secretSaved}
					adminEmail={adminEmail}
				/>

				<AlertTypeChecklist
					selected={draft.alerts}
					onChange={(alerts) => patch({ alerts })}
					error={errors.get("alert_types")}
				/>

				<Field orientation="horizontal">
					<FieldContent>
						<FieldLabel htmlFor="channel-enabled">{form.enabled}</FieldLabel>
					</FieldContent>
					<Switch
						id="channel-enabled"
						checked={draft.enabled}
						onCheckedChange={(enabled) => patch({ enabled })}
					/>
				</Field>

				<div className="flex flex-col items-start gap-4">
					<Button type="button" variant="outline" onClick={sendTest}>
						{test.isPending && <Spinner />}
						{text.test.send}
					</Button>
					<SendTestResult outcome={outcome} />
				</div>
			</FieldGroup>

			{saving.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						<TextWithPaths text={form.saveFailed(saving.error.message)} />
					</AlertDescription>
				</Alert>
			)}

			<div className="flex flex-wrap items-center gap-2">
				<Button type="submit">
					{saving.isPending && <Spinner />}
					{form.save}
				</Button>
				<BackToNotifications />
			</div>
		</form>
	);
}

function factDestination(channel: ChannelDto) {
	const { settings } = channel;
	if (channel.type === ChannelKind.webhook) return settings.url ?? "";
	if (channel.type === ChannelKind.email && !settings.recipients?.length) {
		return settings.smtp_host ?? "";
	}
	return destinationOf(fromChannel(channel));
}

export function ConfigChannelView({ channel }: { channel: ChannelDto }) {
	const test = useTestChannel();
	const queryClient = useQueryClient();
	const outcome: TestOutcome | undefined = test.data
		? test.data.ok
			? {
					ok: true,
					message: text.test.sent(destinationOf(fromChannel(channel))),
				}
			: { ok: false, message: text.test.failed(test.data.message) }
		: test.isError
			? { ok: false, message: text.test.failed(test.error.message) }
			: undefined;

	return (
		<div className="flex flex-col items-start gap-6">
			<Badge variant="outline" className="h-6 text-sm">
				<Lock aria-hidden />
				{text.table.fromConfig}
			</Badge>
			<Alert>
				<Lock aria-hidden />
				<AlertDescription>{text.config.notice}</AlertDescription>
			</Alert>
			<dl className="flex flex-col gap-4">
				<Fact label={form.type}>
					<KindLabel kind={channel.type} />
				</Fact>
				<Fact label={text.config.destination}>
					<span className="font-mono wrap-anywhere">
						<TextWithPaths text={factDestination(channel)} />
					</span>
				</Fact>
				<Fact label={text.table.columns.alertTypes}>
					{channel.alert_types.length === ALERT_TYPES.length
						? text.table.allTypes
						: channel.alert_types
								.map((type) => text.alertTypes[type])
								.join(", ")}
				</Fact>
				{channel.secret_set && (
					<Fact label={text.config.secretSet}>{text.secret.config}</Fact>
				)}
			</dl>
			<div className="flex flex-col items-start gap-4">
				<Button
					type="button"
					variant="outline"
					onClick={() =>
						test.mutate(
							{ data: { channel_id: channel.id } },
							{
								onSettled: () =>
									void queryClient.invalidateQueries({
										queryKey: getListChannelsQueryKey(),
									}),
							},
						)
					}
				>
					{test.isPending && <Spinner />}
					{text.test.send}
				</Button>
				<SendTestResult outcome={outcome} />
			</div>
			<div>
				<BackToNotifications />
			</div>
		</div>
	);
}
