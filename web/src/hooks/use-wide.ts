import { useEffect, useState } from "react";

const WIDE_BREAKPOINT = 1280;

export function useIsWide() {
	const [wide, setWide] = useState(() => window.innerWidth >= WIDE_BREAKPOINT);

	useEffect(() => {
		const mql = window.matchMedia(`(min-width: ${WIDE_BREAKPOINT}px)`);
		const onChange = () => setWide(window.innerWidth >= WIDE_BREAKPOINT);
		mql.addEventListener("change", onChange);
		onChange();
		return () => mql.removeEventListener("change", onChange);
	}, []);

	return wide;
}
