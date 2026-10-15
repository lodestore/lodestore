type Listener = (event: MediaQueryListEvent) => void;

let prefersDark = false;
const listeners = new Set<Listener>();

export function installMatchMedia() {
	window.matchMedia = ((query: string) => ({
		get matches() {
			return query.includes("prefers-color-scheme: dark") && prefersDark;
		},
		media: query,
		onchange: null,
		addEventListener: (_type: string, listener: Listener) =>
			listeners.add(listener),
		removeEventListener: (_type: string, listener: Listener) =>
			listeners.delete(listener),
		addListener: (listener: Listener) => listeners.add(listener),
		removeListener: (listener: Listener) => listeners.delete(listener),
		dispatchEvent: () => false,
	})) as unknown as typeof window.matchMedia;
}

export function setPrefersDark(value: boolean) {
	prefersDark = value;
	for (const listener of [...listeners]) {
		listener({ matches: value } as MediaQueryListEvent);
	}
}
