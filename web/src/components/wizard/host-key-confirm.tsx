import { Check, CircleAlert, Fingerprint, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { checkStorage } from "@/api/gen/endpoints";
import { CodeBlock } from "@/components/code-block";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Spinner } from "@/components/ui/spinner";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { copy } from "@/copy";
import { CHECK_TIMEOUT_MS } from "./draft";
import { text } from "./text";

type Offer = { key: string; fingerprint: string; mismatch: boolean };

export function keyType(key: string) {
	const name = key.split(/\s+/)[0] ?? "";
	return name
		.replace(/^sk-/, "")
		.replace(/^ssh-/, "")
		.split(/[-@]/)[0]
		.toUpperCase();
}

export function HostKeyConfirm({
	endpoint,
	root,
	canGet,
	hostKey,
	onConfirm,
}: {
	endpoint: string;
	root: string;
	canGet: boolean;
	hostKey: string;
	onConfirm: (key: string) => void;
}) {
	const scope = `${endpoint}\n${root}`;
	const [seen, setSeen] = useState(scope);
	const [offer, setOffer] = useState<Offer | null>(null);
	const [loading, setLoading] = useState(false);
	const [error, setError] = useState("");
	const latest = useRef(scope);
	useEffect(() => {
		latest.current = scope;
	}, [scope]);
	if (seen !== scope) {
		setSeen(scope);
		setOffer(null);
		setError("");
		setLoading(false);
	}

	async function get() {
		const asked = scope;
		setLoading(true);
		setError("");
		setOffer(null);
		onConfirm("");
		try {
			const result = await checkStorage(
				{
					location: {
						backend: "sftp",
						options: { endpoint, ...(root ? { root } : {}) },
					},
				},
				{ signal: AbortSignal.timeout(CHECK_TIMEOUT_MS) },
			);
			if (latest.current !== asked) return;
			if (result.host_key) {
				setOffer({ ...result.host_key, mismatch: false });
			} else {
				const failed = result.checks.find((c) => c.status === "fail")?.error;
				setError(
					copy.errors.submit(copy.hostKey.getFailed, failed ?? undefined),
				);
			}
		} catch (e) {
			if (latest.current !== asked) return;
			const message = e instanceof Error ? e.message : "";
			setError(
				copy.errors.submit(
					copy.hostKey.getFailed,
					message.replace(/\.$/, "") || undefined,
				),
			);
		} finally {
			if (latest.current === asked) setLoading(false);
		}
	}

	const matches = offer !== null && hostKey === offer.key;
	const [before, after] = copy.hostKey
		.compare(copy.hostKey.command)
		.split(copy.hostKey.command);

	return (
		<div className="flex flex-col gap-4">
			<div>
				<Button
					type="button"
					variant="outline"
					disabled={!canGet || loading}
					onClick={() => void get()}
				>
					{loading && <Spinner />}
					{loading
						? copy.hostKey.getting
						: offer
							? copy.hostKey.recheck
							: copy.hostKey.get}
				</Button>
			</div>
			{error !== "" && (
				<Alert variant="destructive" role="alert">
					<CircleAlert aria-hidden />
					<AlertDescription>{error}</AlertDescription>
				</Alert>
			)}
			{offer && (
				<Card>
					<CardContent className="flex flex-col gap-4">
						<div className="flex items-center gap-2">
							<Fingerprint aria-hidden className="size-5" />
							<h3 className="font-semibold">{copy.hostKey.title}</h3>
						</div>
						<p className="text-sm text-muted-foreground">
							{before}
							<code className="font-mono text-foreground wrap-anywhere">
								{copy.hostKey.command}
							</code>
							{after}
						</p>
						<CodeBlock
							text={`${offer.fingerprint} (${keyType(offer.key)})`}
							object={text.hostKeyFingerprint}
						/>
						<ToggleGroup
							type="single"
							variant="outline"
							size="lg"
							spacing={0}
							aria-label={copy.hostKey.title}
							value={matches ? "matches" : offer.mismatch ? "mismatch" : ""}
							onValueChange={(value) => {
								if (value === "matches") {
									setOffer({ ...offer, mismatch: false });
									onConfirm(offer.key);
								} else if (value === "mismatch") {
									setOffer({ ...offer, mismatch: true });
									onConfirm("");
								}
							}}
						>
							<ToggleGroupItem
								value="matches"
								className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:font-semibold data-[state=on]:text-primary-strong"
							>
								<Check aria-hidden />
								{copy.hostKey.matches}
							</ToggleGroupItem>
							<ToggleGroupItem
								value="mismatch"
								className="px-4 data-[state=on]:bg-primary-subtle data-[state=on]:font-semibold data-[state=on]:text-primary-strong"
							>
								<X aria-hidden />
								{copy.hostKey.mismatch}
							</ToggleGroupItem>
						</ToggleGroup>
						{offer.mismatch && (
							<Alert variant="destructive" role="alert">
								<CircleAlert aria-hidden />
								<AlertDescription>{copy.hostKey.mismatchBody}</AlertDescription>
							</Alert>
						)}
					</CardContent>
				</Card>
			)}
		</div>
	);
}
