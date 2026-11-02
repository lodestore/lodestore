import { showWaiting, typeScript } from './typing';

const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
const panel = document.getElementById('archp');
const tty = document.getElementById('tty');
const cap = document.getElementById('archcap');
const svg = panel?.querySelector<SVGSVGElement>('.diagram');

const caps = {
	day: 'Agents write encrypted, deduplicated data straight to your storage. The server only tells them when and what.',
	lost: 'The server is gone. Any Linux machine with lode, the bucket and your password or recovery key restores every snapshot.',
} as const;
type Scene = keyof typeof caps;

if (panel && tty && cap && svg) {
	let run = 0;
	const setScene = (scene: Scene) => {
		panel.classList.toggle('lost', scene === 'lost');
		panel.querySelectorAll<HTMLButtonElement>('.seg button').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.s === scene)));
		cap.textContent = caps[scene];
		const id = ++run;
		if (scene === 'lost') void typeScript(tty, () => id === run);
		else showWaiting(tty);
	};

	let auto = true;
	panel.querySelectorAll<HTMLButtonElement>('.seg button').forEach((b) =>
		b.addEventListener('click', () => {
			auto = false;
			setScene(b.dataset.s as Scene);
		}),
	);

	if (!reduce) {
		setScene('day');
		let tab = !document.hidden;
		let seen = false;
		const sync = () => (seen && tab ? svg.unpauseAnimations() : svg.pauseAnimations());
		document.addEventListener('visibilitychange', () => {
			tab = !document.hidden;
			sync();
		});
		let first = true;
		new IntersectionObserver(
			([e]) => {
				seen = e.isIntersecting;
				sync();
				if (e.intersectionRatio >= 0.5 && first) {
					first = false;
					setTimeout(() => auto && setScene('lost'), 2500);
				}
			},
			{ threshold: [0, 0.5] },
		).observe(panel);
	}
}
