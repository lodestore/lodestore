import { Search } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { FieldError } from "@/components/ui/field";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupInput,
} from "@/components/ui/input-group";
import { copy } from "@/copy";

export function FindForm({
	pattern,
	autoFocus,
	error,
	onSearch,
}: {
	pattern: string | undefined;
	autoFocus: boolean;
	error?: string;
	onSearch: (pattern: string) => void;
}) {
	const [value, setValue] = useState(pattern ?? "");
	const input = useRef<HTMLInputElement>(null);
	const errorId = useId();

	useEffect(() => setValue(pattern ?? ""), [pattern]);
	useEffect(() => {
		if (autoFocus) input.current?.focus();
	}, [autoFocus]);

	const trimmed = value.trim();
	return (
		<form
			role="search"
			className="flex flex-col gap-2"
			onSubmit={(event) => {
				event.preventDefault();
				if (trimmed) onSearch(trimmed);
			}}
		>
			<div className="flex gap-4">
				<InputGroup className="h-12">
					<InputGroupAddon>
						<Search aria-hidden />
					</InputGroupAddon>
					<InputGroupInput
						ref={input}
						type="search"
						className="h-full"
						aria-label={copy.find.input.label}
						placeholder={copy.find.input.placeholder}
						autoComplete="off"
						spellCheck={false}
						value={value}
						aria-invalid={error ? true : undefined}
						aria-describedby={error ? errorId : undefined}
						onChange={(event) => setValue(event.target.value)}
					/>
				</InputGroup>
				<Button
					type="submit"
					variant="outline"
					size="lg"
					disabled={trimmed === ""}
				>
					{copy.find.submit}
				</Button>
			</div>
			{error && <FieldError id={errorId}>{error}</FieldError>}
		</form>
	);
}
