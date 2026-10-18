import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { afterAll, afterEach, beforeAll } from "vitest";
import { installMatchMedia, setPrefersDark } from "./media";
import { server } from "./server";

configure({ asyncUtilTimeout: 5000 });
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
Element.prototype.hasPointerCapture ??= () => false;
Element.prototype.setPointerCapture ??= () => {};
Element.prototype.releasePointerCapture ??= () => {};
globalThis.ResizeObserver ??= class {
	observe() {}
	unobserve() {}
	disconnect() {}
};
