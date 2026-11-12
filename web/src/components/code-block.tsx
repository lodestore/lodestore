import { Check, Copy } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { TextWithPaths } from "@/components/path-text";
import { Button } from "@/components/ui/button";
import { copy } from "@/copy";
import { copyText } from "@/lib/clipboard";
import { cn } from "@/lib/utils";

export function CodeBlock({
	text,
	object,
	copyLabel = copy.common.copy(object),
	toastText = copy.common.copied,
	disabled = false,
	large = false,
	className,
}: {
	text: string;
	object: string;
	copyLabel?: string;
	toastText?: string;
	disabled?: boolean;
	large?: boolean;
	className?: string;
}) {
	const [copied, setCopied] = useState(false);
	const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
	useEffect(() => () => clearTimeout(timer.current), []);

	const preRef = useRef<HTMLPreElement>(null);

	async function onCopy() {
		if (!(await copyText(text, preRef.current))) return;
		setCopied(true);
		toast.success(toastText);
		clearTimeout(timer.current);
		timer.current = setTimeout(() => setCopied(false), 2000);
	}

	return (
		<div
			className={cn(
				"relative rounded-md border bg-secondary",
				disabled && "opacity-50",
				className,
			)}
		>
			<pre
				ref={preRef}
				role="region"
				aria-label={object}
				tabIndex={0}
				className={cn(
					"m-0 rounded-md p-4 pr-32 font-mono wrap-anywhere whitespace-pre-wrap",
					large
						? "text-lg tabular-nums tracking-[0.02em]"
						: "text-[0.8125rem] leading-relaxed",
				)}
			>
				<TextWithPaths text={text} />
			</pre>
			<Button
				type="button"
				variant="outline"
				size="sm"
				className="absolute top-2 right-2 bg-background"
				aria-label={copyLabel}
				disabled={disabled}
				onClick={onCopy}
			>
				{copied ? <Check aria-hidden /> : <Copy aria-hidden />}
				{copied ? copy.common.copied : copy.common.copyButton}
			</Button>
		</div>
	);
}
