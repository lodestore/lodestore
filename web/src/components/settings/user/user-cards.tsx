import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { toast } from "sonner";
import { ApiError } from "@/api/fetcher";
import {
	getGetAccountQueryKey,
	getListTokensQueryKey,
	useChangePassword,
	useGetAccount,
	useListTokens,
	useUpdateAccount,
} from "@/api/gen/endpoints";
import { Fact } from "@/components/fact";
import { RowField } from "@/components/policies/form-field";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const TOAST_MS = 4000;
const PASSWORD_TOAST_MS = 6000;
const MIN_PASSWORD = 12;
const EMAIL_SHAPE = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

const account = copy.user.account;

function CardTitleH2({ children }: { children: string }) {
	return (
		<CardHeader>
			<CardTitle>
				<h2>{children}</h2>
			</CardTitle>
		</CardHeader>
	);
}

export function EmailCard() {
	const queryClient = useQueryClient();
	const current = useGetAccount();
	const update = useUpdateAccount();
	const [draft, setDraft] = useState<string>();
	const [invalid, setInvalid] = useState(false);

	if (current.isPending) {
		return (
			<Card>
				<CardTitleH2>{account.title}</CardTitleH2>
				<CardContent className="flex flex-col gap-4">
					<Skeleton className="h-10 w-48" />
					<Skeleton className="h-10 w-full" />
				</CardContent>
			</Card>
		);
	}
	if (current.isError) {
		return (
			<Card>
				<CardTitleH2>{account.title}</CardTitleH2>
				<CardContent className="flex flex-col items-start gap-4">
					<Alert variant="destructive">
						<AlertDescription>{account.loadFailed}</AlertDescription>
					</Alert>
					<Button variant="outline" onClick={() => void current.refetch()}>
						{copy.errors.retry}
					</Button>
				</CardContent>
			</Card>
		);
	}

	const saved = current.data.email ?? "";
	const value = draft ?? saved;
	const dirty = value !== saved;

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (update.isPending || !dirty) return;
		const email = value.trim();
		if (email !== "" && !EMAIL_SHAPE.test(email)) {
			setInvalid(true);
			return;
		}
		update.mutate(
			{ data: { email: email === "" ? null : email } },
			{
				onSuccess: (next) => {
					queryClient.setQueryData(getGetAccountQueryKey(), next);
					setDraft(undefined);
					toast.success(account.saved, { duration: TOAST_MS });
				},
			},
		);
	}

	return (
		<Card>
			<CardTitleH2>{account.title}</CardTitleH2>
			<form onSubmit={onSubmit} noValidate className="contents">
				<CardContent className="flex flex-col gap-4">
					<dl>
						<Fact label={account.username}>
							<span className="font-mono">{current.data.username}</span>
						</Fact>
					</dl>
					<RowField
						label={account.email}
						helper={account.emailHelper}
						error={invalid ? account.invalidEmail : undefined}
					>
						{(field) => (
							<Input
								{...field}
								name="email"
								type="email"
								autoComplete="email"
								value={value}
								onChange={(e) => {
									setDraft(e.target.value);
									setInvalid(false);
								}}
							/>
						)}
					</RowField>
					{update.isError && (
						<Alert variant="destructive" role="alert">
							<AlertDescription>
								{copy.errors.submit(account.saveAction, update.error.message)}
							</AlertDescription>
						</Alert>
					)}
				</CardContent>
				<CardFooter>
					<Button type="submit" variant="outline" disabled={!dirty}>
						{update.isPending && <Spinner />}
						{account.save}
					</Button>
				</CardFooter>
			</form>
		</Card>
	);
}

const password = copy.user.password;

export function PasswordCard() {
	const queryClient = useQueryClient();
	const change = useChangePassword();
	const tokens = useListTokens({ state: "active" });
	const [revoke, setRevoke] = useState(true);
	const now = Date.now();
	const activeTokens = (tokens.data ?? []).filter(
		(t) => t.expires_at == null || t.expires_at > now,
	).length;
	const [current, setCurrent] = useState("");
	const [next, setNext] = useState("");
	const [confirm, setConfirm] = useState("");
	const [wrong, setWrong] = useState(false);
	const [tooShort, setTooShort] = useState(false);
	const [mismatch, setMismatch] = useState(false);
	const [failure, setFailure] = useState<string>();
	const filled = current !== "" && next !== "" && confirm !== "";

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (change.isPending || !filled) return;
		const short = next.length < MIN_PASSWORD;
		const different = next !== confirm;
		setTooShort(short);
		setMismatch(!short && different);
		setWrong(false);
		setFailure(undefined);
		if (short || different) return;
		change.mutate(
			{
				data: {
					current_password: current,
					new_password: next,
					revoke_tokens: activeTokens > 0 && revoke,
				},
			},
			{
				onSuccess: () => {
					void queryClient.invalidateQueries({
						queryKey: getListTokensQueryKey(),
					});
					setCurrent("");
					setNext("");
					setConfirm("");
					toast.success(password.done, { duration: PASSWORD_TOAST_MS });
				},
				onError: (e) => {
					if (e instanceof ApiError && e.code === "wrong_password") {
						setWrong(true);
					} else if (e instanceof ApiError && e.code === "weak_password") {
						setTooShort(true);
					} else if (e instanceof ApiError && e.status === 429) {
						setFailure(copy.login.rateLimited(e.retryAfter ?? 60));
					} else if (e instanceof ApiError && e.status === 503) {
						setFailure(copy.login.busy(e.retryAfter ?? 5));
					} else {
						setFailure(copy.errors.submit(password.action, e.message));
					}
				},
			},
		);
	}

	return (
		<Card>
			<CardTitleH2>{password.title}</CardTitleH2>
			<form onSubmit={onSubmit} noValidate className="contents">
				<CardContent className="flex flex-col gap-4">
					<RowField
						label={password.current}
						error={wrong ? password.wrongCurrent : undefined}
					>
						{(field) => (
							<Input
								{...field}
								name="current-password"
								type="password"
								autoComplete="current-password"
								value={current}
								onChange={(e) => {
									setCurrent(e.target.value);
									setWrong(false);
								}}
							/>
						)}
					</RowField>
					<RowField
						label={password.next}
						helper={copy.setup.passwordRule}
						error={tooShort ? password.tooShort : undefined}
					>
						{(field) => (
							<Input
								{...field}
								name="new-password"
								type="password"
								autoComplete="new-password"
								value={next}
								onChange={(e) => {
									setNext(e.target.value);
									setTooShort(false);
								}}
							/>
						)}
					</RowField>
					<RowField
						label={password.confirm}
						error={mismatch ? password.mismatch : undefined}
					>
						{(field) => (
							<Input
								{...field}
								name="confirm-password"
								type="password"
								autoComplete="new-password"
								value={confirm}
								onChange={(e) => {
									setConfirm(e.target.value);
									setMismatch(false);
								}}
							/>
						)}
					</RowField>
					{activeTokens > 0 && (
						<div className="flex items-center gap-2">
							<Checkbox
								id="revoke-tokens"
								checked={revoke}
								onCheckedChange={(value) => setRevoke(value === true)}
							/>
							<Label htmlFor="revoke-tokens" className="font-normal">
								{password.revokeTokens(activeTokens)}
							</Label>
						</div>
					)}
					{failure && (
						<Alert variant="destructive" role="alert">
							<AlertDescription>{failure}</AlertDescription>
						</Alert>
					)}
				</CardContent>
				<CardFooter>
					<Button
						type="submit"
						variant={filled ? "default" : "outline"}
						disabled={!filled}
					>
						{change.isPending && <Spinner />}
						{password.submit}
					</Button>
				</CardFooter>
			</form>
		</Card>
	);
}
