const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const byId = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function onScreen(el: Element) {
	let seen = true;
	let tab = !document.hidden;
	new IntersectionObserver(([e]) => (seen = e.isIntersecting)).observe(el);
	document.addEventListener('visibilitychange', () => (tab = !document.hidden));
	return () => seen && tab;
}

const ok =
	'<svg class="icon" viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="m8.5 12 2.5 2.5 4.5-5"/></svg>';
const spin = '<svg class="icon spin" viewBox="0 0 24 24"><path d="M21 12a9 9 0 1 1-6.2-8.6"/></svg>';

function rotateWord(rot: HTMLElement) {
	const rotating = onScreen(rot);
	let ri = 0;
	setInterval(() => {
		if (!rotating()) return;
		ri++;
		rot.style.transition = '';
		rot.style.transform = `translateY(-${ri * 1.05}em)`;
		if (ri === 4)
			setTimeout(() => {
				rot.style.transition = 'none';
				rot.style.transform = 'translateY(0)';
				ri = 0;
			}, 700);
	}, 2600);
}

async function playDemo(demo: HTMLElement) {
	const body = byId('dbody');
	const cur = byId('cursor');
	const row = byId('drow');
	const status = byId('dst');
	const head = byId('dhead');
	const sub = byId('dsub');
	const add = byId('dadd');
	const copy = byId('dcopy');
	const toast = byId('dtoast');
	const lastOk = byId('dls');
	const size = byId('dsz');
	const running = onScreen(demo);

	const moveTo = (el: HTMLElement) => {
		const b = body.getBoundingClientRect();
		const r = el.getBoundingClientRect();
		cur.style.left = `${r.left - b.left + r.width * 0.6}px`;
		cur.style.top = `${r.top - b.top + r.height * 0.55}px`;
	};
	const reset = () => {
		demo.classList.remove('modal');
		row.classList.remove('show');
		toast.classList.remove('show');
		status.textContent = '';
		lastOk.textContent = '—';
		size.textContent = '—';
		head.textContent = 'All 4 hosts protected';
		sub.textContent = 'Newest backup 3 minutes ago';
		copy.textContent = 'Copy';
		copy.classList.remove('done', 'press');
		cur.style.left = '70%';
		cur.style.top = '88%';
	};

	demo.classList.add('live');
	for (;;) {
		reset();
		while (!running()) await sleep(500);
		await sleep(1400);
		moveTo(add);
		await sleep(900);
		add.classList.add('press');
		await sleep(180);
		add.classList.remove('press');
		demo.classList.add('modal');
		await sleep(900);
		moveTo(copy);
		await sleep(900);
		copy.classList.add('press');
		copy.textContent = 'Copied';
		copy.classList.add('done');
		await sleep(1100);
		demo.classList.remove('modal');
		cur.style.left = '85%';
		cur.style.top = '20%';
		await sleep(700);
		row.classList.add('show');
		status.innerHTML = `<span class="st neu">${spin}Joining</span>`;
		head.textContent = '4 of 5 hosts protected';
		await sleep(1300);
		status.innerHTML = `<span class="st run">${spin}First backup</span><span class="bar"><i id="dbar"></i></span>`;
		await sleep(60);
		byId('dbar').style.width = '100%';
		await sleep(2900);
		status.innerHTML = `<span class="st ok">${ok}Protected</span>`;
		lastOk.textContent = 'just now';
		size.textContent = '2.4 TiB';
		head.textContent = 'All 5 hosts protected';
		sub.textContent = 'Newest backup just now';
		toast.classList.add('show');
		await sleep(3800);
	}
}

const demo = document.getElementById('demo');
const rot = document.getElementById('rot');
if (demo && rot && !reduce) {
	rotateWord(rot);
	void playDemo(demo);
}
