const reduce = matchMedia('(prefers-reduced-motion: reduce)');
const videos = [...document.querySelectorAll<HTMLVideoElement>('video[data-src]')];
const onScreen = new Set<HTMLVideoElement>();
const root = document.documentElement;

const isCurrentTheme = (v: HTMLVideoElement) =>
	v.classList.contains(root.dataset.theme === 'dark' ? 'only-dark' : 'only-light');

const choice = new Map<Element, boolean>();
const figureOf = (v: HTMLVideoElement) => v.closest('figure') ?? v;
const wanted = (v: HTMLVideoElement) => choice.get(figureOf(v)) ?? !reduce.matches;

function sync() {
	for (const v of videos) {
		v.setAttribute('aria-label', `${wanted(v) ? 'Pause' : 'Play'}: ${v.dataset.label ?? ''}`);
		const play = wanted(v) && !document.hidden && onScreen.has(v) && isCurrentTheme(v);
		if (play) {
			if (!v.getAttribute('src')) v.src = v.dataset.src ?? '';
			v.play().catch(() => {});
		} else if (!v.paused) {
			v.pause();
		}
	}
}

const io = new IntersectionObserver((entries) => {
	for (const e of entries) {
		if (e.isIntersecting) onScreen.add(e.target as HTMLVideoElement);
		else onScreen.delete(e.target as HTMLVideoElement);
	}
	sync();
});
videos.forEach((v) => io.observe(v));

function toggle(v: HTMLVideoElement) {
	choice.set(figureOf(v), v.paused);
	sync();
}
for (const v of videos) {
	v.addEventListener('click', () => toggle(v));
	v.addEventListener('keydown', (e) => {
		if (e.key !== 'Enter' && e.key !== ' ') return;
		e.preventDefault();
		toggle(v);
	});
}

document.addEventListener('visibilitychange', sync);
reduce.addEventListener('change', sync);
new MutationObserver(sync).observe(root, { attributes: true, attributeFilter: ['data-theme'] });
