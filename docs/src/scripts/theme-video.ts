const reduce = matchMedia('(prefers-reduced-motion: reduce)');
const videos = [...document.querySelectorAll<HTMLVideoElement>('video[data-src]')];
const onScreen = new Set<HTMLVideoElement>();
const root = document.documentElement;

const isCurrentTheme = (v: HTMLVideoElement) =>
	v.classList.contains(root.dataset.theme === 'dark' ? 'only-dark' : 'only-light');

function sync() {
	for (const v of videos) {
		const play = !reduce.matches && !document.hidden && onScreen.has(v) && isCurrentTheme(v);
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

document.addEventListener('visibilitychange', sync);
reduce.addEventListener('change', sync);
new MutationObserver(sync).observe(root, { attributes: true, attributeFilter: ['data-theme'] });
