import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ChevronDown, CircleCheck, Server, TriangleAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
	getListHostsQueryKey,
	useCreateJoinToken,
	useGetToken,
} from "@/api/gen/endpoints";
import type { JoinTokenResponse, TokenHost } from "@/api/gen/model";
import { CodeBlock } from "@/components/code-block";
import { LabelsEditor } from "@/components/labels-editor";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Collapsible,
	CollapsibleContent,
	CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
	EmptyTitle,
} from "@/components/ui/empty";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";
import { type LabelRow, validateLabels } from "@/lib/labels";
import { infoQuery, repositoriesQuery } from "@/lib/session";

const POLL_MS = 3000;

type Issued = { response: JoinTokenResponse; offset: number };

function formatRemaining(ms: number) {
	const total = Math.max(0, Math.floor(ms / 1000));
	const minutes = Math.floor(total / 60);
	return `${String(minutes).padStart(2, "0")}:${String(total % 60).padStart(2, "0")}`;
}

export function AddHostPanel({
	repositoryId,
	onConnected,
}: {
	repositoryId?: string;
	onConnected?: (host: TokenHost) => void;
}) {
	const queryClient = useQueryClient();
	const repositories = useQuery(repositoriesQuery);
	const info = useQuery(infoQuery);
	const create = useCreateJoinToken({ mutation: { gcTime: 0 } });

	const [chosen, setChosen] = useState<string>();
	const [labels, setLabels] = useState<LabelRow[]>([]);
	const [issued, setIssued] = useState<Issued>();
	const [now, setNow] = useState(() => Date.now());
	const notified = useRef(false);

	const list = repositories.data ?? [];
	const selectedId = repositoryId ?? chosen ?? list[0]?.id;
	const selected = list.find((r) => r.id === selectedId);
	const validation = validateLabels(labels);

	const token = useGetToken(issued?.response.id ?? "", {
		query: {
			enabled: issued !== undefined,
			gcTime: 0,
			refetchInterval: (query) => {
				const data = query.state.data;
				return data?.status === "expired" || data?.host?.status === "online"
					? false
					: POLL_MS;
			},
		},
	});

	useEffect(() => {
		if (!issued) return;
		const timer = setInterval(() => setNow(Date.now()), 1000);
		return () => clearInterval(timer);
	}, [issued]);

	const host = token.data?.host;
	const connected = host?.status === "online";
	const remaining = issued
		? issued.response.expires_at - (now + issued.offset)
		: 0;
	const expired =
		!connected &&
		issued !== undefined &&
		(token.data?.status === "expired" || remaining <= 0);

	useEffect(() => {
		if (!connected || !host || notified.current) return;
		notified.current = true;
		void queryClient.invalidateQueries({ queryKey: getListHostsQueryKey() });
		onConnected?.(host);
	}, [connected, host, onConnected, queryClient]);

	function generate() {
		if (!selectedId || !validation.valid) return;
		notified.current = false;
		create.mutate(
			{
				data: {
					repository_id: selectedId,
					labels: labels.filter((row) => row.key !== ""),
				},
			},
			{
				onSuccess: (response) => {
					setIssued({
						response,
						offset: response.server_time - Date.now(),
					});
					setNow(Date.now());
					create.reset();
				},
			},
		);
	}

	if (repositories.isPending) {
		return <Spinner className="mx-auto my-6" />;
	}

	if (repositories.isError) {
		return (
			<Alert variant="destructive" role="alert">
				<AlertDescription>
					{copy.errors.list("repositories")}{" "}
					<Button
						variant="link"
						className="h-auto p-0"
						onClick={() => repositories.refetch()}
					>
						{copy.errors.retry}
					</Button>
				</AlertDescription>
			</Alert>
		);
	}

	if (list.length === 0) {
		return (
			<Empty>
				<EmptyHeader>
					<EmptyMedia variant="icon">
						<Server />
					</EmptyMedia>
					<EmptyTitle>{copy.addHost.noRepoHeading}</EmptyTitle>
					<EmptyDescription>{copy.addHost.noRepoBody}</EmptyDescription>
				</EmptyHeader>
				<EmptyContent>
					<Button asChild>
						<Link to="/settings/repositories/new">
							{copy.addHost.noRepoButton}
						</Link>
					</Button>
				</EmptyContent>
			</Empty>
		);
	}

	const noBinaries =
		info.data !== undefined && info.data.agent_arches.length === 0;
	const readOnly = issued !== undefined;

	return (
		<div className="flex flex-col gap-4">
			{noBinaries && (
				<Alert role="alert" className="text-warning">
					<TriangleAlert aria-hidden />
					<AlertDescription>{copy.addHost.noAgentBinaries}</AlertDescription>
				</Alert>
			)}

			{info.data && (
				<p className="text-sm text-muted-foreground">
					{copy.addHost.helper(info.data.grpc_public_address)}
				</p>
			)}

			<Field>
				<FieldLabel htmlFor="add-host-repository">
					{copy.addHost.repository}
				</FieldLabel>
				{repositoryId !== undefined || list.length === 1 || readOnly ? (
					<p id="add-host-repository" className="text-sm font-semibold">
						{selected?.name ?? selectedId}
					</p>
				) : (
					<Select value={selectedId} onValueChange={setChosen}>
						<SelectTrigger id="add-host-repository" className="w-full">
							<SelectValue />
						</SelectTrigger>
						<SelectContent>
							{list.map((r) => (
								<SelectItem key={r.id} value={r.id}>
									{r.name}
								</SelectItem>
							))}
						</SelectContent>
					</Select>
				)}
			</Field>

			{readOnly ? (
				labels.some((row) => row.key !== "") && (
					<p className="font-mono text-sm">
						{labels
							.filter((row) => row.key !== "")
							.map((row) => `${row.key}=${row.value}`)
							.join(", ")}
					</p>
				)
			) : (
				<Field>
					<FieldLabel>{copy.addHost.labelsTitle}</FieldLabel>
					<LabelsEditor value={labels} onChange={setLabels} />
					<FieldDescription>{copy.addHost.labelsHelper}</FieldDescription>
				</Field>
			)}

			{create.isError && (
				<Alert variant="destructive" role="alert">
					<AlertDescription>
						{copy.errors.submit("generate the token", create.error.message)}
					</AlertDescription>
				</Alert>
			)}

			{!readOnly && (
				<Button
					className="self-start"
					disabled={create.isPending || !selectedId || !validation.valid}
					onClick={generate}
				>
					{create.isPending && <Spinner />}
					{copy.addHost.generate}
				</Button>
			)}

			{issued && (
				<>
					<CodeBlock
						text={issued.response.command}
						object={copy.addHost.commandObject}
						copyLabel={copy.addHost.copyCommand}
						toastText={copy.addHost.commandCopied}
						disabled={expired}
					/>
					<Alert>
						<AlertDescription>{copy.addHost.shownOnce}</AlertDescription>
					</Alert>
					{!expired && !connected && (
						<p
							aria-live="off"
							className="text-sm text-muted-foreground tabular-nums"
						>
							{copy.addHost.expiresIn(formatRemaining(remaining))}
						</p>
					)}
					{connected && host ? (
						<p role="status" className="flex items-center gap-2 text-sm">
							<CircleCheck aria-hidden className="size-4 text-success" />
							{copy.addHost.connected(host.hostname, host.version)}
						</p>
					) : expired ? (
						<div className="flex flex-wrap items-center gap-3">
							<p role="alert" className="text-sm">
								{copy.addHost.tokenExpired}
							</p>
							<Button variant="secondary" size="sm" onClick={generate}>
								{copy.addHost.generateNew}
							</Button>
						</div>
					) : (
						<p
							role="status"
							className="flex items-center gap-2 text-sm text-muted-foreground"
						>
							<Spinner />
							{copy.addHost.waiting}
						</p>
					)}
				</>
			)}

			<Collapsible>
				<CollapsibleTrigger asChild>
					<Button variant="ghost" size="sm" className="-ml-2">
						<ChevronDown aria-hidden />
						{copy.addHost.installOptionsTitle}
					</Button>
				</CollapsibleTrigger>
				<CollapsibleContent className="text-sm text-muted-foreground">
					{copy.addHost.installOptions}
				</CollapsibleContent>
			</Collapsible>
		</div>
	);
}
