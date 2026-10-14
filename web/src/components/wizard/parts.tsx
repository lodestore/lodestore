import { Eye, EyeOff } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardFooter } from "@/components/ui/card";
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
		<Card className="w-full">
			<CardContent className="flex flex-col gap-6">{children}</CardContent>
			<CardFooter className="justify-between gap-3">
				{onBack ? (
					<Button
						type="button"
						variant="ghost"
						disabled={backDisabled}
						onClick={onBack}
					>
						{copy.wizard.back}
					</Button>
				) : (
					<span />
				)}
				<div className="flex flex-wrap items-center justify-end gap-3">
					{actions}
				</div>
			</CardFooter>
		</Card>
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
					aria-label={`${shown ? "Hide" : "Show"} ${what}`}
					onClick={() => setShown((v) => !v)}
				>
					{shown ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
				</InputGroupButton>
			</InputGroupAddon>
		</InputGroup>
	);
}
