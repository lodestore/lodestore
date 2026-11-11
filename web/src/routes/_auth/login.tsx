import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useRouter } from "@tanstack/react-router";
import { CircleAlert, Eye, EyeOff, Info } from "lucide-react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useLogin } from "@/api/gen/endpoints";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupButton,
	InputGroupInput,
} from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import {
	isSetupRequired,
	redirectIfSignedIn,
	safeNext,
	sessionQuery,
	statusQuery,
} from "@/lib/session";

export const Route = createFileRoute("/_auth/login")({
	validateSearch: (search: Record<string, unknown>) => ({
		next: safeNext(search.next),
		expired:
			search.expired === true || search.expired === "true" ? true : undefined,
	}),
	beforeLoad: ({ context: { queryClient } }) => redirectIfSignedIn(queryClient),
	component: LoginPage,
});

function LoginPage() {
	const { next, expired } = Route.useSearch();
	const router = useRouter();
	const queryClient = useQueryClient();
	const status = useQuery(statusQuery);
	const login = useLogin();

	const [username, setUsername] = useState("");
	const [password, setPassword] = useState("");
	const [showPassword, setShowPassword] = useState(false);
	const [error, setError] = useState<string>();
	const [wait, setWait] = useState(0);
	const [busy, setBusy] = useState(false);
	const [failures, setFailures] = useState(0);
	const passwordRef = useRef<HTMLInputElement>(null);

	useEffect(() => {
		if (failures > 0 && !login.isPending) passwordRef.current?.focus();
	}, [failures, login.isPending]);

	useEffect(() => {
		if (wait <= 0) return;
		const timer = setTimeout(() => setWait((n) => n - 1), 1000);
		return () => clearTimeout(timer);
	}, [wait]);

	const noAdmin =
		status.data?.setup_required === true ||
		(login.error !== null && isSetupRequired(login.error));
	const blocked = login.isPending || wait > 0;

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (blocked) return;
		setError(undefined);
		login.mutate(
			{ data: { username, password } },
			{
				onSuccess: async () => {
					try {
						await queryClient.fetchQuery({ ...sessionQuery, staleTime: 0 });
					} catch (e) {
						setError(copy.errors.submit("sign in", (e as Error).message));
						return;
					}
					router.history.push(next ?? "/hosts");
				},
				onError: (e) => {
					setPassword("");
					setFailures((n) => n + 1);
					if (e instanceof ApiError && e.status === 429) {
						setBusy(false);
						setWait(e.retryAfter ?? 60);
						return;
					}
					if (e instanceof ApiError && e.status === 503) {
						setBusy(true);
						setWait(e.retryAfter ?? 5);
						return;
					}
					if (e instanceof ApiError && e.status === 401) {
						setError(copy.login.error);
						return;
					}
					if (isSetupRequired(e)) return;
					setError(copy.errors.submit("sign in", e.message));
				},
			},
		);
	}

	return (
		<>
			<div className="flex flex-col gap-2">
				<h1 data-page-title className="text-2xl font-semibold">
					{copy.login.title}
				</h1>
				<p className="text-base text-muted-foreground">{copy.auth.loginLead}</p>
			</div>
			<form onSubmit={onSubmit} noValidate>
				<FieldGroup className="gap-6">
					{expired && (
						<Alert>
							<Info aria-hidden />
							<AlertDescription>{copy.login.sessionExpired}</AlertDescription>
						</Alert>
					)}
					{noAdmin && (
						<Alert>
							<Info aria-hidden />
							<AlertDescription>{copy.login.noAdmin}</AlertDescription>
						</Alert>
					)}
					<Field>
						<FieldLabel htmlFor="username">{copy.auth.username}</FieldLabel>
						<Input
							id="username"
							name="username"
							autoComplete="username"
							autoFocus
							disabled={login.isPending}
							value={username}
							onChange={(e) => setUsername(e.target.value)}
						/>
					</Field>
					<Field>
						<FieldLabel htmlFor="password">{copy.auth.password}</FieldLabel>
						<InputGroup>
							<InputGroupInput
								id="password"
								name="password"
								ref={passwordRef}
								type={showPassword ? "text" : "password"}
								autoComplete="current-password"
								disabled={login.isPending}
								value={password}
								onChange={(e) => setPassword(e.target.value)}
							/>
							<InputGroupAddon align="inline-end">
								<InputGroupButton
									size="icon-xs"
									aria-label={
										showPassword
											? copy.auth.hidePassword
											: copy.auth.showPassword
									}
									onClick={() => setShowPassword((v) => !v)}
								>
									{showPassword ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
								</InputGroupButton>
							</InputGroupAddon>
						</InputGroup>
					</Field>
					{(error || wait > 0) && (
						<Alert variant="destructive" role="alert">
							<CircleAlert aria-hidden />
							<AlertDescription>
								{wait > 0
									? busy
										? copy.login.busy(wait)
										: copy.login.rateLimited(wait)
									: error}
							</AlertDescription>
						</Alert>
					)}
					<Button type="submit" size="lg" className="w-full" disabled={blocked}>
						{login.isPending && <Spinner />}
						{copy.login.button}
					</Button>
				</FieldGroup>
			</form>
		</>
	);
}
