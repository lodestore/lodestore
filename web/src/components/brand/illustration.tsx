import { useTheme } from "@/components/theme/theme-provider";

export type IllustrationName =
	| "auth-hero"
	| "hosts-empty"
	| "repos-empty"
	| "preview-overview"
	| "preview-policies"
	| "preview-jobs"
	| "preview-restore"
	| "preview-host-tabs";

const urls = import.meta.glob<string>("../../assets/illustrations/*.webp", {
	eager: true,
	query: "?url",
	import: "default",
});

export function Illustration({
	name,
	width,
	height,
	className,
}: {
	name: IllustrationName;
	width: number;
	height: number;
	className?: string;
}) {
	const { resolvedTheme } = useTheme();
	const src = urls[`../../assets/illustrations/${name}.${resolvedTheme}.webp`];
	if (!src) return null;
	return (
		<img
			src={src}
			alt=""
			width={width}
			height={height}
			decoding="async"
			className={className}
		/>
	);
}
