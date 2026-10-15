export function LogoMark({
	className,
	title,
}: {
	className?: string;
	title?: string;
}) {
	return (
		<svg
			viewBox="0 0 32 32"
			className={className}
			role={title ? "img" : undefined}
			aria-hidden={title ? undefined : true}
			xmlns="http://www.w3.org/2000/svg"
		>
			{title ? <title>{title}</title> : null}
			<path
				d="M16 2.8L27.43 9.4L27.43 22.6L16 29.2L4.57 22.6L4.57 9.4Z"
				fill="var(--mark-plate)"
				stroke="var(--mark-edge)"
				strokeWidth="1.6"
				strokeLinejoin="round"
			/>
			<path
				d="M16 6.4L18 14L25.6 16L18 18L16 25.6L14 18L6.4 16L14 14Z"
				fill="var(--mark-star)"
			/>
			<path
				d="M16 12.4L16.9 15.1L19.6 16L16.9 16.9L16 19.6L15.1 16.9L12.4 16L15.1 15.1Z"
				fill="var(--mark-core)"
			/>
		</svg>
	);
}
