import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import { setCsrfToken } from "@/api/fetcher";
import { useLogout } from "@/api/gen/endpoints";
import { copy } from "@/copy";
import { isSignedOut, sessionQuery } from "@/lib/session";

export function useSignOut() {
	const navigate = useNavigate();
	const queryClient = useQueryClient();
	const logout = useLogout();

	async function finish() {
		setCsrfToken(undefined);
		queryClient.removeQueries({ queryKey: sessionQuery.queryKey });
		await navigate({
			to: "/login",
			search: { next: undefined, expired: undefined },
		});
		queryClient.clear();
	}

	return function signOut() {
		logout.mutate(undefined, {
			onSuccess: finish,
			onError: (e) =>
				isSignedOut(e)
					? finish()
					: toast.error(copy.errors.submit("sign out", e.message)),
		});
	};
}
