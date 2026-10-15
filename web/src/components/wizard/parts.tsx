import { Eye, EyeOff } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupButton,
	InputGroupInput,
} from "@/components/ui/input-group";
import { copy } from "@/copy";

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
			<footer className="sticky bottom-0 flex min-h-16 shrink-0 items-center justify-between gap-2 border-t py-2 bg-background">
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
