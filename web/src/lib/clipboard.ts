import { toast } from "sonner";
import { copy } from "@/copy";

function copyViaSelection(text: string, source?: HTMLElement | null) {
	const previous = document.activeElement;
	const field = document.createElement("textarea");
	field.value = text;
	field.readOnly = true;
	field.style.cssText = "position:fixed;top:0;left:0;opacity:0;";
	(source?.parentElement ?? document.body).appendChild(field);
	try {
		field.focus({ preventScroll: true });
		field.select();
		field.setSelectionRange(0, text.length);
		return document.execCommand("copy");
	} catch {
		return false;
	} finally {
		field.remove();
		if (previous instanceof HTMLElement) previous.focus();
	}
}

export async function copyText(
	text: string,
	source?: HTMLElement | null,
): Promise<boolean> {
	if (navigator.clipboard) {
		try {
			await navigator.clipboard.writeText(text);
			return true;
		} catch (error) {
			console.debug("The Clipboard API refused the write", error);
		}
	}
	if (copyViaSelection(text, source)) return true;
	if (source) window.getSelection()?.selectAllChildren(source);
	toast.error(
		source ? copy.common.copyFailed : copy.common.copyFailedUnselected,
	);
	return false;
}
