import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterAll, afterEach, beforeAll } from "vitest";
import { installMatchMedia, setPrefersDark } from "./media";
import { server } from "./server";

installMatchMedia();
beforeAll(() => server.listen({ onUnhandledFrame: "error" }));
afterEach(() => {
	cleanup();
	server.resetHandlers();
	setPrefersDark(false);
	document.documentElement.classList.remove("dark");
	localStorage.removeItem("lodestore.theme");
});
afterAll(() => server.close());
window.scrollTo = () => {};
Element.prototype.scrollIntoView ??= () => {};
globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};
