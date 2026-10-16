/** Turns a server message into a sentence followed by a space, or "" when there is none. */
export function sentence(message?: string | null) {
	const text = message?.trim().replace(/[.\s]+$/, "");
	if (!text) return "";
	return `${text[0].toUpperCase()}${text.slice(1)}. `;
}
