import {
	createContext,
	type ReactNode,
	useCallback,
	useContext,
	useEffect,
	useMemo,
	useState,
} from "react";

export const THEME_STORAGE_KEY = "lodestore.theme";

export type Theme = "light" | "dark" | "system";

const DARK_QUERY = "(prefers-color-scheme: dark)";

type ThemeContextValue = {
	theme: Theme;
	resolvedTheme: "light" | "dark";
	setTheme: (theme: Theme) => void;
};

const ThemeContext = createContext<ThemeContextValue | null>(null);

const isTheme = (value: unknown): value is Theme =>
	value === "light" || value === "dark" || value === "system";

function readStoredTheme(): Theme {
	try {
		const stored = localStorage.getItem(THEME_STORAGE_KEY);
		return isTheme(stored) ? stored : "system";
	} catch {
		return "system";
	}
}

export function ThemeProvider({ children }: { children: ReactNode }) {
	const [theme, setThemeState] = useState<Theme>(readStoredTheme);
	const [systemDark, setSystemDark] = useState(
		() => window.matchMedia(DARK_QUERY).matches,
	);

	useEffect(() => {
		const query = window.matchMedia(DARK_QUERY);
		const onChange = (event: MediaQueryListEvent) =>
			setSystemDark(event.matches);
		setSystemDark(query.matches);
		query.addEventListener("change", onChange);
		return () => query.removeEventListener("change", onChange);
	}, []);

	const resolvedTheme =
		theme === "system" ? (systemDark ? "dark" : "light") : theme;

	useEffect(() => {
		const root = document.documentElement;
		root.classList.toggle("dark", resolvedTheme === "dark");
		root.style.colorScheme = resolvedTheme;
	}, [resolvedTheme]);

	const setTheme = useCallback((next: Theme) => {
		if (!isTheme(next)) return;
		setThemeState(next);
		try {
			localStorage.setItem(THEME_STORAGE_KEY, next);
		} catch {
			return;
		}
	}, []);

	const value = useMemo(
		() => ({ theme, resolvedTheme, setTheme }),
		[theme, resolvedTheme, setTheme],
	);
	return <ThemeContext value={value}>{children}</ThemeContext>;
}

export function useTheme(): ThemeContextValue {
	const context = useContext(ThemeContext);
	if (!context) throw new Error("useTheme must be used inside ThemeProvider");
	return context;
}
