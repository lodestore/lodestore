import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { CircleAlert } from "lucide-react";
import { type FormEvent, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { useCreateAdmin, verify } from "@/api/gen/endpoints";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
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
import { useFocusHeading } from "@/lib/focus-heading";
import { redirectIfSignedIn, sessionQuery, statusQuery } from "@/lib/session";

const MIN_PASSWORD = 12;

export const Route = createFileRoute("/setup")({
	validateSearch: (search: Record<string, unknown>) => ({
		token: typeof search.token === "string" ? search.token : undefined,
	}),
	beforeLoad: ({ context: { queryClient } }) => redirectIfSignedIn(queryClient),
	component: SetupPage,
});

function SetupPage() {
	useFocusHeading();
	const { token } = Route.useSearch();
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const create = useCreateAdmin();
	const check = useQuery({
		queryKey: ["setup/verify", token],
		queryFn: async ({ signal }) => {
			await verify({ token: token ?? "" }, undefined, signal);
			return true;
		},
		enabled: token !== undefined && !create.isSuccess,
		retry: false,
		staleTime: Number.POSITIVE_INFINITY,
		gcTime: 0,
		refetchOnWindowFocus: false,
	});

	const [username, setUsername] = useState("admin");
	const [password, setPassword] = useState("");
	const [confirm, setConfirm] = useState("");
	const [passwordInvalid, setPasswordInvalid] = useState(false);
	const [mismatch, setMismatch] = useState(false);
	const [error, setError] = useState<string>();
	const [linkRejected, setLinkRejected] = useState(false);

	const verifyFailed = check.error instanceof ApiError && check.error.status;
	const invalidLink =
		token === undefined ||
		linkRejected ||
		(check.isError && verifyFailed === 404);

	function onSubmit(event: FormEvent) {
		event.preventDefault();
		if (create.isPending || token === undefined) return;
		const tooShort = password.length < MIN_PASSWORD;
		const different = password !== confirm;
		setPasswordInvalid(tooShort);
		setMismatch(!tooShort && different);
		setError(undefined);
		if (tooShort || different) return;
		create.mutate(
			{ data: { token, username, password } },
			{
				onSuccess: async () => {
					queryClient.removeQueries({ queryKey: statusQuery.queryKey });
					await queryClient.fetchQuery({ ...sessionQuery, staleTime: 0 });
					await navigate({ to: "/welcome", replace: true });
				},
				onError: (e) => {
					setPassword("");
					setConfirm("");
					if (e instanceof ApiError && e.status === 409) {
						void navigate({
							to: "/login",
							search: { next: undefined, expired: undefined },
							replace: true,
						});
						return;
					}
					if (e instanceof ApiError && e.status === 404) {
						setLinkRejected(true);
						return;
					}
					if (e instanceof ApiError && e.status === 429) {
						setError(copy.login.rateLimited(e.retryAfter ?? 60));
						return;
					}
					setError(copy.errors.submit("create the administrator", e.message));
				},
			},
		);
	}

	return (
		<main className="flex min-h-svh justify-center px-4 pt-16">
			<div className="w-full max-w-[400px]">
				<h1 className="mb-6 text-center text-2xl font-semibold">
					{copy.setup.title}
				</h1>
				<Card>
					<CardContent>
						{invalidLink ? (
							<Alert variant="destructive">
								<CircleAlert aria-hidden />
								<AlertDescription>{copy.setup.invalidLink}</AlertDescription>
							</Alert>
						) : check.isError ? (
							<Alert variant="destructive">
								<CircleAlert aria-hidden />
								<AlertDescription>
									{check.error instanceof ApiError && check.error.status === 429
										? copy.login.rateLimited(check.error.retryAfter ?? 60)
										: copy.errors.submit(
												"check the setup link",
												check.error.message,
											)}
								</AlertDescription>
							</Alert>
						) : check.isPending ? (
							<div className="flex justify-center py-6">
								<Spinner className="size-6" />
							</div>
						) : (
							<form onSubmit={onSubmit} noValidate>
								<FieldGroup>
									<Field>
										<FieldLabel htmlFor="username">Username</FieldLabel>
										<Input
											id="username"
											name="username"
											autoComplete="username"
											disabled={create.isPending}
											value={username}
											onChange={(e) => setUsername(e.target.value)}
										/>
									</Field>
									<Field>
										<FieldLabel htmlFor="password">Password</FieldLabel>
										<Input
											id="password"
											name="password"
											type="password"
											autoComplete="new-password"
											autoFocus
											aria-describedby="password-hint"
											aria-invalid={passwordInvalid}
											disabled={create.isPending}
											value={password}
											onChange={(e) => setPassword(e.target.value)}
										/>
										<FieldDescription
											id="password-hint"
											className={passwordInvalid ? "text-destructive" : ""}
										>
											{copy.setup.passwordRule}
										</FieldDescription>
									</Field>
									<Field>
										<FieldLabel htmlFor="confirm">
											{copy.setup.confirmPassword}
										</FieldLabel>
										<Input
											id="confirm"
											name="confirm"
											type="password"
											autoComplete="new-password"
											aria-describedby={mismatch ? "confirm-error" : undefined}
											aria-invalid={mismatch}
											disabled={create.isPending}
											value={confirm}
											onChange={(e) => setConfirm(e.target.value)}
										/>
										{mismatch && (
											<FieldError id="confirm-error">
												{copy.setup.passwordMismatch}
											</FieldError>
										)}
									</Field>
									{error && (
										<Alert variant="destructive">
											<CircleAlert aria-hidden />
											<AlertDescription>{error}</AlertDescription>
										</Alert>
									)}
									<Button
										type="submit"
										className="w-full"
										disabled={create.isPending}
									>
										{create.isPending && <Spinner />}
										{copy.setup.button}
									</Button>
								</FieldGroup>
							</form>
						)}
					</CardContent>
				</Card>
			</div>
		</main>
	);
}
