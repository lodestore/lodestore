import { Eye, EyeOff } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
	Field,
	FieldDescription,
	FieldGroup,
	FieldLabel,
	FieldLegend,
	FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupButton,
	InputGroupInput,
} from "@/components/ui/input-group";
import { Textarea } from "@/components/ui/textarea";
import { copy } from "@/copy";
import { text } from "./text";

export function StepCard({
	children,
	onBack,
	backDisabled,
	actions,
}: {
	children: ReactNode;
	onBack?: () => void;
	backDisabled?: boolean;
	actions: ReactNode;
}) {
	return (
		<div className="flex w-full flex-col gap-8">
			<div className="flex flex-col gap-6">{children}</div>
			<footer className="sticky bottom-0 flex min-h-16 shrink-0 flex-wrap items-center justify-between gap-2 border-t py-2 bg-background">
				{onBack ? (
					<Button
						type="button"
						variant="outline"
						disabled={backDisabled}
						onClick={onBack}
					>
						{copy.wizard.previousStep}
					</Button>
				) : (
					<span />
				)}
				<div className="flex flex-wrap items-center justify-end gap-2">
					{actions}
				</div>
			</footer>
		</div>
	);
}

export function StepHeading({ children }: { children: string }) {
	const ref = useRef<HTMLHeadingElement>(null);
	useEffect(() => ref.current?.focus(), []);
	return (
		<h2 ref={ref} tabIndex={-1} className="text-lg font-semibold outline-none">
			{children}
		</h2>
	);
}

export function SecretInput({
	id,
	what,
	value,
	onChange,
	autoComplete = "off",
	invalid,
	describedBy,
	disabled,
}: {
	id: string;
	what: string;
	value: string;
	onChange: (value: string) => void;
	autoComplete?: string;
	invalid?: boolean;
	describedBy?: string;
	disabled?: boolean;
}) {
	const [shown, setShown] = useState(false);
	return (
		<InputGroup>
			<InputGroupInput
				id={id}
				name={id}
				type={shown ? "text" : "password"}
				autoComplete={autoComplete}
				spellCheck={false}
				disabled={disabled}
				aria-invalid={invalid || undefined}
				aria-describedby={describedBy}
				value={value}
				onChange={(event) => onChange(event.target.value)}
			/>
			<InputGroupAddon align="inline-end">
				<InputGroupButton
					size="icon-xs"
					aria-label={
						shown ? copy.common.hideSecret(what) : copy.common.showSecret(what)
					}
					onClick={() => setShown((v) => !v)}
				>
					{shown ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
				</InputGroupButton>
			</InputGroupAddon>
		</InputGroup>
	);
}

export function TextField({
	id,
	label,
	help,
	placeholder,
	value,
	onChange,
	disabled,
}: {
	id: string;
	label: string;
	help?: string;
	placeholder?: string;
	value: string;
	onChange: (value: string) => void;
	disabled: boolean;
}) {
	return (
		<Field>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			<Input
				id={id}
				name={id}
				className="font-mono"
				autoComplete="off"
				autoCapitalize="off"
				spellCheck={false}
				disabled={disabled}
				placeholder={placeholder}
				aria-describedby={help ? `${id}-help` : undefined}
				value={value}
				onChange={(event) => onChange(event.target.value)}
			/>
			{help && (
				<FieldDescription id={`${id}-help`} className="wrap-anywhere">
					{help}
				</FieldDescription>
			)}
		</Field>
	);
}

export function KeyFields({
	prefix,
	legend,
	keyId,
	secret,
	onKeyId,
	onSecret,
	disabled,
}: {
	prefix: string;
	legend: string;
	keyId: string;
	secret: string;
	onKeyId: (value: string) => void;
	onSecret: (value: string) => void;
	disabled: boolean;
}) {
	return (
		<FieldSet>
			<FieldLegend variant="label" className="sr-only">
				{legend}
			</FieldLegend>
			<FieldGroup>
				<Field>
					<FieldLabel htmlFor={`${prefix}-key-id`}>
						<span className="sr-only">{`${legend}: `}</span>
						{text.accessKeyId}
					</FieldLabel>
					<Input
						id={`${prefix}-key-id`}
						name={`${prefix}-key-id`}
						className="font-mono"
						autoComplete="off"
						autoCapitalize="off"
						spellCheck={false}
						disabled={disabled}
						value={keyId}
						onChange={(event) => onKeyId(event.target.value)}
					/>
				</Field>
				<Field>
					<FieldLabel htmlFor={`${prefix}-key-secret`}>
						<span className="sr-only">{`${legend}: `}</span>
						{text.secretAccessKey}
					</FieldLabel>
					<SecretInput
						id={`${prefix}-key-secret`}
						what={`${legend.toLowerCase()} secret`}
						disabled={disabled}
						value={secret}
						onChange={onSecret}
					/>
				</Field>
			</FieldGroup>
		</FieldSet>
	);
}

export function SecretField({
	id,
	label,
	help,
	value,
	onChange,
	disabled,
}: {
	id: string;
	label: string;
	help?: string;
	value: string;
	onChange: (value: string) => void;
	disabled: boolean;
}) {
	return (
		<Field>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			<SecretInput
				id={id}
				what={label.toLowerCase()}
				disabled={disabled}
				describedBy={help ? `${id}-help` : undefined}
				value={value}
				onChange={onChange}
			/>
			{help && (
				<FieldDescription id={`${id}-help`} className="wrap-anywhere">
					{help}
				</FieldDescription>
			)}
		</Field>
	);
}

export function PrivateKeyField({
	id,
	label,
	value,
	onChange,
	disabled,
}: {
	id: string;
	label: string;
	value: string;
	onChange: (value: string) => void;
	disabled: boolean;
}) {
	return (
		<Field>
			<FieldLabel htmlFor={id}>{label}</FieldLabel>
			<Textarea
				id={id}
				name={id}
				rows={5}
				className="font-mono"
				autoComplete="off"
				autoCapitalize="off"
				spellCheck={false}
				disabled={disabled}
				value={value}
				onChange={(event) => onChange(event.target.value)}
			/>
		</Field>
	);
}
