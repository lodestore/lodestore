import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useRouter } from "@tanstack/react-router";
import { CircleAlert, Eye, EyeOff, Info } from "lucide-react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useLogin } from "@/api/gen/endpoints";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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
import { useFocusHeading } from "@/lib/focus-heading";
import {
	isSetupRequired,
	redirectIfSignedIn,
	safeNext,
	sessionQuery,
	statusQuery,
} from "@/lib/session";

export const Route = createFileRoute("/login")({
	validateSearch: (search: Record<string, unknown>) => ({
		next: safeNext(search.next),
		expired:
			search.expired === true || search.expired === "true" ? true : undefined,
	}),
	beforeLoad: ({ context: { queryClient } }) => redirectIfSignedIn(queryClient),
	component: LoginPage,
});

function LoginPage() {
	useFocusHeading();
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
					await queryClient.fetchQuery({ ...sessionQuery, staleTime: 0 });
					router.history.push(next ?? "/hosts");
				},
				onError: (e) => {
					setPassword("");
					setFailures((n) => n + 1);
					if (e instanceof ApiError && e.status === 429) {
						setWait(e.retryAfter ?? 60);
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
		<main className="flex min-h-svh justify-center px-4 pt-16">
			<div className="w-full max-w-[400px]">
				<h1 className="mb-6 text-center text-2xl font-semibold">Lodestore</h1>
				<Card>
					<CardContent>
						<form onSubmit={onSubmit} noValidate>
							<FieldGroup>
								{expired && (
									<Alert>
										<Info aria-hidden />
										<AlertDescription>
											{copy.login.sessionExpired}
										</AlertDescription>
									</Alert>
								)}
								{noAdmin && (
									<Alert>
										<Info aria-hidden />
										<AlertDescription>{copy.login.noAdmin}</AlertDescription>
									</Alert>
								)}
								<Field>
									<FieldLabel htmlFor="username">Username</FieldLabel>
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
									<FieldLabel htmlFor="password">Password</FieldLabel>
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
													showPassword ? "Hide password" : "Show password"
												}
												onClick={() => setShowPassword((v) => !v)}
											>
												{showPassword ? (
													<EyeOff aria-hidden />
												) : (
													<Eye aria-hidden />
												)}
											</InputGroupButton>
										</InputGroupAddon>
									</InputGroup>
								</Field>
								{(error || wait > 0) && (
									<Alert variant="destructive" role="alert">
										<CircleAlert aria-hidden />
										<AlertDescription>
											{wait > 0 ? copy.login.rateLimited(wait) : error}
										</AlertDescription>
									</Alert>
								)}
								<Button type="submit" className="w-full" disabled={blocked}>
									{login.isPending && <Spinner />}
									{copy.login.button}
								</Button>
							</FieldGroup>
						</form>
					</CardContent>
				</Card>
			</div>
		</main>
	);
}
