export type TermLine = ['c' | '$' | 'o', string];

export const restoreScript: TermLine[] = [
	['c', '# the server is gone; you still have the bucket and the key'],
	['$', 'export LODE_REPOSITORY="s3:https://s3.example.com/backups"'],
	['$', 'export LODE_PASSWORD_FILE=./repository-key'],
	['$', 'lode snapshots'],
	['o', '3f9a1c2e  2026-11-01 03:00:12  pg-main-01  /var/lib/postgresql'],
	['o', '8be04d71  2026-11-02 03:00:09  pg-main-01  /var/lib/postgresql'],
	['$', 'lode restore latest --target ./restored --verify'],
];

const waiting = '# waiting: nothing to restore today';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function el(tag: string, cls: string, text = '') {
	const node = document.createElement(tag);
	if (cls) node.className = cls;
	node.textContent = text;
	return node;
}

export function showWaiting(tty: HTMLElement) {
	const note = el('div', '');
	note.append(el('span', 'c', waiting), el('span', 'cur'));
	tty.replaceChildren(note);
}

export async function typeScript(tty: HTMLElement, alive: () => boolean) {
	const instant = matchMedia('(prefers-reduced-motion: reduce)').matches;
	const wait = (ms: number) => (instant ? Promise.resolve() : sleep(ms));
	tty.replaceChildren();
	for (const [kind, text] of restoreScript) {
		if (!alive()) return;
		const line = el('div', '');
		tty.append(line);
		if (kind === 'c') {
			line.append(el('span', 'c', text));
			await wait(400);
		} else if (kind === 'o') {
			line.textContent = text;
			await wait(140);
		} else {
			const typed = el('span', '');
			line.append(el('span', 'p', '$ '), typed);
			for (const ch of text) {
				if (!alive()) return;
				typed.textContent += ch;
				await wait(17);
			}
			await wait(350);
		}
	}
	if (alive()) tty.lastElementChild?.append(el('span', 'cur'));
}
