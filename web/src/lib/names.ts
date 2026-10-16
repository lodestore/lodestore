const SLASH = 0x2f;

const UNSAFE = /[\p{Cc}\p{Cf}\u{2028}\u{2029}]/gu;
const ZWJ = "\u{200D}";
const EMOJI_BEFORE = /\p{Extended_Pictographic}(?:\p{EMod}|\u{FE0F})?$/u;
const EMOJI_AFTER = /^\p{Extended_Pictographic}/u;

/** Escapes control, format and bidi characters; keeps ZWJ inside emoji sequences. */
export function displayName(text: string) {
	return text.replace(UNSAFE, (char, offset: number) => {
		if (
			char === ZWJ &&
			EMOJI_BEFORE.test(text.slice(Math.max(0, offset - 4), offset)) &&
			EMOJI_AFTER.test(text.slice(offset + 1))
		) {
			return char;
		}
		const code = char.codePointAt(0) ?? 0;
		if (code < 0x20) return String.fromCodePoint(0x2400 + code);
		if (code === 0x7f) return String.fromCodePoint(0x2421);
		return `[U+${code.toString(16).toUpperCase().padStart(4, "0")}]`;
	});
}

/** Escapes `text` and wraps it in FSI/PDI so RTL letters cannot reorder the sentence around it. */
export const isolate = (text: string) => `⁨${displayName(text)}⁩`;

/** Decodes unpadded base64url; malformed input decodes to no bytes. */
export function decodeB64(text: string) {
	const std = text.replaceAll("-", "+").replaceAll("_", "/");
	try {
		const binary = atob(std + "=".repeat((4 - (std.length % 4)) % 4));
		return Uint8Array.from(binary, (char) => char.charCodeAt(0));
	} catch {
		return new Uint8Array();
	}
}

export function encodeB64(bytes: Uint8Array) {
	let binary = "";
	for (const byte of bytes) binary += String.fromCharCode(byte);
	return btoa(binary)
		.replaceAll("+", "-")
		.replaceAll("/", "_")
		.replace(/=+$/, "");
}

export const decodeText = (bytes: Uint8Array) =>
	new TextDecoder().decode(bytes);

export function pathParts(pathB64: string) {
	const bytes = decodeB64(pathB64);
	const parts: Uint8Array[] = [];
	let start = 0;
	for (let i = 0; i <= bytes.length; i += 1) {
		if (i < bytes.length && bytes[i] !== SLASH) continue;
		if (i > start) parts.push(bytes.slice(start, i));
		start = i + 1;
	}
	return parts;
}

export function absolutePath(parts: Uint8Array[]) {
	if (parts.length === 0) return encodeB64(Uint8Array.of(SLASH));
	const bytes: number[] = [];
	for (const part of parts) bytes.push(SLASH, ...part);
	return encodeB64(Uint8Array.from(bytes));
}

export const urlPath = (parts: Uint8Array[]) =>
	parts.length === 0 ? "" : absolutePath(parts);
