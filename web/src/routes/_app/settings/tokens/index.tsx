import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { useListTokens } from "@/api/gen/endpoints";
import type { ApiTokenItem } from "@/api/gen/model";
import { CreateTokenDialog } from "@/components/settings/create-token-dialog";
import { RevokeTokenDialog } from "@/components/settings/revoke-token-dialog";
import { SettingsNav } from "@/components/settings/settings-nav";
import { TokensTable } from "@/components/settings/tokens-table";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";

type TokensSearch = { create?: true; state?: "active" | "revoked" };

export const Route = createFileRoute("/_app/settings/tokens/")({
	validateSearch: (search: Record<string, unknown>): TokensSearch => ({
		create: [true, 1, "1", "true"].includes(search.create as never)
			? true
			: undefined,
		state: search.state === "revoked" ? "revoked" : undefined,
	}),
	component: TokensPage,
});

function focusRow(id: string) {
	const row = document.querySelector<HTMLElement>(
		`[data-token-row="${CSS.escape(id)}"]`,
	);
	(row ?? document.querySelector<HTMLElement>("[data-page-title]"))?.focus();
}

function restoreFocus(event: Event, id: string | undefined) {
	event.preventDefault();
	const target =
		id === undefined
			? null
			: document.querySelector<HTMLElement>(
					`[data-token-actions="${CSS.escape(id)}"]`,
				);
	(target ?? document.querySelector<HTMLElement>("[data-page-title]"))?.focus();
}

function TokensPage() {
	const { create, state } = Route.useSearch();
	const navigate = useNavigate();
	const [creating, setCreating] = useState(create === true);
	const [revoking, setRevoking] = useState<ApiTokenItem>();
	const origin = useRef<string>(undefined);
	const filter = state ?? "active";
	const tokens = useListTokens({ state: filter });

	useEffect(() => {
		if (!create) return;
		setCreating(true);
		void navigate({
			to: "/settings/tokens",
			search: (prev) => ({ ...prev, create: undefined }),
			replace: true,
		});
	}, [create, navigate]);

	return (
		<div className="flex flex-col gap-6 pb-8">
			<header className="flex flex-col gap-2">
				<div className="flex items-center justify-between gap-4">
					<h1 data-page-title className="text-2xl font-semibold">
						{copy.tokens.title}
					</h1>
					<Button onClick={() => setCreating(true)}>
						{copy.tokens.create}
					</Button>
				</div>
				<p className="text-sm text-muted-foreground">{copy.tokens.lead}</p>
			</header>

			<SettingsNav />

			<TokensTable
				tokens={tokens.data}
				filter={filter}
				isError={tokens.isError}
				onFilterChange={(next) =>
					void navigate({
						to: "/settings/tokens",
						search: (prev) => ({
							...prev,
							state: next === "revoked" ? "revoked" : undefined,
						}),
						replace: true,
					})
				}
				onRetry={() => tokens.refetch()}
				onCreate={() => setCreating(true)}
				onRevoke={(token) => {
					origin.current = token.id;
					setRevoking(token);
				}}
			/>

			<CreateTokenDialog
				open={creating}
				onOpenChange={setCreating}
				onFocusRow={focusRow}
			/>
			<RevokeTokenDialog
				token={revoking}
				onClose={(revoked) => {
					if (revoked) origin.current = undefined;
					setRevoking(undefined);
				}}
				onCloseAutoFocus={(event) => restoreFocus(event, origin.current)}
			/>
		</div>
	);
}
