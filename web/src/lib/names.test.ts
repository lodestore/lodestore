import { describe, expect, it } from "vitest";
import {
	absolutePath,
	decodeB64,
	decodeText,
	displayName,
	encodeB64,
	isolate,
	pathParts,
	urlPath,
} from "@/lib/names";

const ch = (code: number) => String.fromCodePoint(code);

describe("displayName", () => {
	it("shows control characters as pictures", () => {
		expect(displayName("a\nb")).toBe(`a${ch(0x240a)}b`);
		expect(displayName("tab\there")).toBe(`tab${ch(0x2409)}here`);
		expect(displayName("nul\x00")).toBe(`nul${ch(0x2400)}`);
		expect(displayName("del\x7f")).toBe(`del${ch(0x2421)}`);
	});

	it("spells out bidi and invisible characters", () => {
		expect(displayName(`invoice${ch(0x202e)}gpj.exe`)).toBe(
			"invoice[U+202E]gpj.exe",
		);
		expect(displayName(`a${ch(0x2066)}b${ch(0x2069)}`)).toBe(
			"a[U+2066]b[U+2069]",
		);
		expect(displayName(`x${ch(0x200e)}y${ch(0x200f)}`)).toBe(
			"x[U+200E]y[U+200F]",
		);
		expect(displayName(`z${ch(0x85)}`)).toBe("z[U+0085]");
	});

	it("spells out every format character", () => {
		for (const code of [0x061c, 0x2060, 0x2064, 0x206a, 0xfff9, 0xe0041]) {
			expect(displayName(`a${ch(code)}b`)).toBe(
				`a[U+${code.toString(16).toUpperCase().padStart(4, "0")}]b`,
			);
		}
	});

	it("keeps emoji and CJK untouched", () => {
		const name = "備份 😀 バックアップ 한국어.txt";
		expect(displayName(name)).toBe(name);
	});

	it("keeps the joiner inside emoji sequences only", () => {
		const family = "\u{1F468}‍\u{1F469}‍\u{1F467}";
		const coder = "\u{1F469}\u{1F3FD}‍\u{1F4BB}";
		expect(displayName(family)).toBe(family);
		expect(displayName(coder)).toBe(coder);
		expect(displayName("a‍b")).toBe("a[U+200D]b");
	});
});

describe("isolate", () => {
	it("escapes the text and wraps it in first-strong isolates", () => {
		expect(isolate(`web${ch(0x202e)}1`)).toBe("⁨web[U+202E]1⁩");
	});
});

describe("paths", () => {
	const text = (b64: string) => pathParts(b64).map(decodeText);

	it("round-trips bytes through base64url", () => {
		const bytes = Uint8Array.from([0xff, 0xfe, 0x2f, 0x00, 0x41]);
		expect(decodeB64(encodeB64(bytes))).toEqual(bytes);
		expect(encodeB64(Uint8Array.of(0xfb, 0xff))).toBe("-_8");
	});

	it("splits on slashes and ignores empty parts", () => {
		expect(text(encodeB64(new TextEncoder().encode("/etc//ssh/")))).toEqual([
			"etc",
			"ssh",
		]);
		expect(pathParts("")).toEqual([]);
	});

	it("treats malformed base64 as the root", () => {
		expect(pathParts("a")).toEqual([]);
		expect(pathParts("abc!")).toEqual([]);
	});

	it("builds absolute and URL paths", () => {
		const parts = pathParts(encodeB64(new TextEncoder().encode("/a/b")));
		expect(decodeText(decodeB64(absolutePath(parts)))).toBe("/a/b");
		expect(decodeText(decodeB64(absolutePath([])))).toBe("/");
		expect(urlPath([])).toBe("");
		expect(urlPath(parts)).toBe(absolutePath(parts));
	});
});
