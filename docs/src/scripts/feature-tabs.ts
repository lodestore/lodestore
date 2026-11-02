const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

const section = document.getElementById('feat');
const list = document.getElementById('tabs');
const tabs = [...document.querySelectorAll<HTMLButtonElement>('.tab')];
const panes = [...document.querySelectorAll<HTMLElement>('.pane')];
const all = (sel: string) => [...document.querySelectorAll<HTMLElement>(sel)];
const one = (sel: string) => document.querySelector<HTMLElement>(sel)!;

type Pause = (ms: number) => Promise<void>;

const reset = () => {
	all('#v0 [data-d]').forEach((x) => x.classList.remove('dup'));
	all('#v0 .dd-store .hx, #v1 div, #v3 .res div, #v4 > div').forEach((x) => x.classList.remove('in'));
	one('#v2').classList.remove('yaml');
	one('#q').textContent = '';
};

const play: Record<number, (p: Pause) => Promise<void>> = {
	0: async (p) => {
		await p(700);
		for (const x of all('#v0 [data-d]')) {
			x.classList.add('dup');
			await p(140);
		}
		for (const x of all('#v0 .dd-store .hx')) {
			x.classList.add('in');
			await p(160);
		}
	},
	1: async (p) => {
		for (const x of all('#v1 div')) {
			await p(520);
			x.classList.add('in');
		}
	},
	2: async (p) => {
		await p(2200);
		one('#v2').classList.add('yaml');
	},
	3: async (p) => {
		for (const ch of 'nginx.conf') {
			one('#q').textContent += ch;
			await p(90);
		}
		for (const x of all('#v3 .res div')) {
			await p(260);
			x.classList.add('in');
		}
	},
	4: async (p) => {
		for (const x of all('#v4 > div')) {
			await p(700);
			x.classList.add('in');
		}
	},
};

if (section && list && tabs.length === panes.length) {
	let run = 0;
	const show = (i: number) => {
		tabs.forEach((t, k) => {
			t.setAttribute('aria-selected', String(k === i));
			t.tabIndex = k === i ? 0 : -1;
		});
		panes.forEach((p, k) => p.classList.toggle('on', k === i));
		if (reduce) return;
		const id = ++run;
		reset();
		const pause: Pause = async (ms) => {
			await sleep(ms);
			if (id !== run) throw new Error('cancelled');
		};
		play[i](pause).catch(() => {});
	};

	const stop = () => list.classList.add('stopped');
	tabs.forEach((t, k) =>
		t.addEventListener('click', () => {
			stop();
			show(k);
		}),
	);
	list.addEventListener('keydown', (e) => {
		const at = tabs.indexOf(document.activeElement as HTMLButtonElement);
		const to = ({ ArrowRight: at + 1, ArrowLeft: at - 1, Home: 0, End: tabs.length - 1 } as Record<string, number>)[e.key];
		if (at < 0 || to === undefined) return;
		e.preventDefault();
		stop();
		const next = (to + tabs.length) % tabs.length;
		show(next);
		tabs[next].focus();
	});

	if (!reduce) {
		reset();
		list.addEventListener('animationend', (e) => {
			if ((e.target as HTMLElement).classList.contains('prog')) show((tabs.findIndex((t) => t.tabIndex === 0) + 1) % tabs.length);
		});

		let seen = false;
		let tab = !document.hidden;
		const sync = () => list.classList.toggle('paused', !(seen && tab));
		document.addEventListener('visibilitychange', () => {
			tab = !document.hidden;
			sync();
		});
		let first = true;
		new IntersectionObserver(
			([e]) => {
				seen = e.isIntersecting;
				sync();
				if (e.isIntersecting && first) {
					first = false;
					show(0);
				}
			},
			{ rootMargin: '0px 0px -30% 0px' },
		).observe(section);
	}
}
