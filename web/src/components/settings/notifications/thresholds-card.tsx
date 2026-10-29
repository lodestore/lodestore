import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { toast } from "sonner";
import {
	getGetAlertThresholdsQueryKey,
	useGetAlertThresholds,
	usePutAlertThresholds,
} from "@/api/gen/endpoints";
import type { AlertThresholds } from "@/api/gen/model";
import { RowField } from "@/components/policies/form-field";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Card,
	CardContent,
	CardDescription,
	CardFooter,
	CardHeader,
	CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupInput,
	InputGroupText,
} from "@/components/ui/input-group";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { copy } from "@/copy";

const text = copy.notifications.thresholds;
const TOAST_MS = 4000;
const UNIT_SECONDS: Record<string, number> = { s: 1, m: 60, h: 3600, d: 86400 };

function seconds(value: string) {
	const trimmed = value.trim();
	if (!/^(\d+[smhd])+$/.test(trimmed)) return null;
	let total = 0;
	for (const [, amount, unit] of trimmed.matchAll(/(\d+)([smhd])/g)) {
		total += Number(amount) * UNIT_SECONDS[unit];
	}
	return total;
}

function Shell({ children }: { children: React.ReactNode }) {
	return (
		<Card className="@container">
			<CardHeader>
				<CardTitle>
					<h2>{text.title}</h2>
				</CardTitle>
				<CardDescription>{text.lead}</CardDescription>
			</CardHeader>
			{children}
		</Card>
	);
}

function ThresholdsForm({ saved }: { saved: AlertThresholds }) {
	const queryClient = useQueryClient();
	const put = usePutAlertThresholds();
	const [offline, setOffline] = useState<string>();
	const [days, setDays] = useState<string>();
	const [submitted, setSubmitted] = useState(false);

	const offlineValue = offline ?? saved.offline_after;
	const daysValue = days ?? String(saved.cert_days);
	const dirty =
		offlineValue !== saved.offline_after ||
		daysValue !== String(saved.cert_days);

	const floor = seconds(saved.offline_floor) ?? 0;
	const offlineSeconds = seconds(offlineValue);
	const offlineError =
		submitted && (offlineSeconds === null || offlineSeconds < floor)
			? text.offlineInvalid(saved.offline_floor)
			: undefined;
	const daysNumber = Number(daysValue);
	const daysError =
		submitted &&
		(!/^\d+$/.test(daysValue.trim()) ||
			daysNumber < 1 ||
			daysNumber > saved.cert_days_max)
			? text.certInvalid(saved.cert_days_max)
			: undefined;

	function discard() {
		setOffline(undefined);
		setDays(undefined);
		setSubmitted(false);
		put.reset();
	}

	function submit(event: React.FormEvent) {
		event.preventDefault();
		if (!dirty || put.isPending) return;
		setSubmitted(true);
		if (
			offlineSeconds === null ||
			offlineSeconds < floor ||
			!/^\d+$/.test(daysValue.trim()) ||
			daysNumber < 1 ||
			daysNumber > saved.cert_days_max
		) {
			return;
		}
		put.mutate(
			{ data: { offline_after: offlineValue.trim(), cert_days: daysNumber } },
			{
				onSuccess: (next) => {
					queryClient.setQueryData(getGetAlertThresholdsQueryKey(), next);
					toast.success(text.saved, { duration: TOAST_MS });
					discard();
				},
			},
		);
	}

	return (
		<Shell>
			<form onSubmit={submit} noValidate className="contents">
				<CardContent className="flex flex-col gap-6">
					<div className="grid grid-cols-1 gap-6 @min-[560px]:grid-cols-2">
						<RowField label={text.offline} error={offlineError}>
							{(a11y) => (
								<Input
									{...a11y}
									name="offline_after"
									autoComplete="off"
									autoCapitalize="off"
									spellCheck={false}
									placeholder={text.offlinePlaceholder}
									className="h-10 font-mono"
									value={offlineValue}
									onChange={(event) => setOffline(event.target.value)}
								/>
							)}
						</RowField>
						<RowField label={text.cert} error={daysError}>
							{(a11y) => (
								<InputGroup className="h-10">
									<InputGroupInput
										{...a11y}
										name="cert_days"
										inputMode="numeric"
										autoComplete="off"
										placeholder={text.certPlaceholder}
										className="font-mono"
										value={daysValue}
										onChange={(event) => setDays(event.target.value)}
									/>
									<InputGroupAddon align="inline-end">
										<InputGroupText>{text.days}</InputGroupText>
									</InputGroupAddon>
								</InputGroup>
							)}
						</RowField>
					</div>
					{put.isError && (
						<Alert variant="destructive" role="alert">
							<AlertDescription>
								{text.saveFailed(put.error.message)}
							</AlertDescription>
						</Alert>
					)}
				</CardContent>
				<CardFooter className="flex-wrap gap-2">
					<Button
						type="submit"
						variant={dirty ? "default" : "outline"}
						disabled={!dirty}
					>
						{put.isPending && <Spinner />}
						{text.save}
					</Button>
					{dirty && (
						<Button type="button" variant="outline" onClick={discard}>
							{text.discard}
						</Button>
					)}
				</CardFooter>
			</form>
		</Shell>
	);
}

export function ThresholdsCard() {
	const query = useGetAlertThresholds();
	if (query.isPending) {
		return (
			<Shell>
				<CardContent className="grid grid-cols-1 gap-6 @min-[560px]:grid-cols-2">
					<Skeleton className="h-16" />
					<Skeleton className="h-16" />
				</CardContent>
			</Shell>
		);
	}
	if (query.isError) {
		return (
			<Shell>
				<CardContent className="flex flex-col items-start gap-4">
					<Alert variant="destructive" role="alert">
						<AlertDescription>{text.loadFailed}</AlertDescription>
					</Alert>
					<Button variant="outline" onClick={() => void query.refetch()}>
						{copy.errors.retry}
					</Button>
				</CardContent>
			</Shell>
		);
	}
	return <ThresholdsForm saved={query.data} />;
}
