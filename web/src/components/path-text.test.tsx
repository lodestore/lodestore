import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { PathText, TextWithPaths } from "@/components/path-text";

const units = (container: HTMLElement) =>
	[...container.querySelectorAll("span.inline-block")].map(
		(el) => el.textContent,
	);

describe("PathText", () => {
	it("breaks only after slashes and keeps the original text", () => {
		const value = "/var/lodestore/restore/2026-10-16-e8769bc2";
		const { container } = render(<PathText value={value} />);
		expect(container).toHaveTextContent(value);
		expect(container.textContent).toBe(value);
		expect(units(container)).toEqual([
			"/var/",
			"lodestore/",
			"restore/",
			"2026-10-16-e8769bc2",
		]);
		expect(container.querySelectorAll("wbr")).toHaveLength(3);
		expect(container.querySelector("span.inline-block wbr")).toBeNull();
		expect(container.querySelector("wbr:last-child")).toBeNull();
	});

	it("keeps a host prefix with its slash", () => {
		const { container } = render(<PathText value="web-1:/srv/x" />);
		expect(units(container)).toEqual(["web-1:/", "srv/", "x"]);
	});

	it("renders an empty value as nothing", () => {
		const { container } = render(<PathText value="" />);
		expect(container.textContent).toBe("");
	});
});

describe("TextWithPaths", () => {
	it("wraps only the slash tokens", () => {
		const text =
			"The agent on web-1 cannot write to /var/x-y/z. Choose another folder.";
		const { container } = render(<TextWithPaths text={text} />);
		expect(container.textContent).toBe(text);
		expect(units(container)).toEqual(["/var/", "x-y/", "z."]);
	});

	it("renders text without slashes as one text node", () => {
		const { container } = render(<TextWithPaths text="Nothing to do here" />);
		const root = container.firstElementChild;
		expect(root?.childNodes).toHaveLength(1);
		expect(root?.textContent).toBe("Nothing to do here");
	});
});
