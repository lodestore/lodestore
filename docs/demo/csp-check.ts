import { type Browser, type BrowserContext, chromium, type Page } from "playwright";

const origin = "https://lodestore.dev:18443";
const themes = ["light", "dark"] as const;

type Violation = { directive: string; blocked: string; source: string };
type Probed = Window & { __csp?: Violation[]; __cspProbe1?: number; __cspProbe2?: number };

const failures: string[] = [];
const fail = (page: string, reason: string) => failures.push(`csp check failed: ${page}: ${reason}`);

const collect = () => {
	const w = window as Probed;
	w.__csp = [];
	document.addEventListener("securitypolicyviolation", (e) => {
		w.__csp?.push({ directive: e.violatedDirective, blocked: e.blockedURI, source: e.sourceFile || e.documentURI });
	});
};

const probe = () =>
	new Promise<void>((resolve) => {
		const script = document.createElement("script");
		script.textContent = "window.__cspProbe2 = 1";
		document.body.append(script);
		const img = document.createElement("img");
		img.setAttribute("onload", "window.__cspProbe1 = 1");
		img.addEventListener("load", () => resolve());
		img.addEventListener("error", () => resolve());
		img.src = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='1' height='1'/%3E";
		document.body.append(img);
	});

async function newContext(browser: Browser, theme: (typeof themes)[number]): Promise<BrowserContext> {
	const context = await browser.newContext({
		ignoreHTTPSErrors: true,
		viewport: { width: 1280, height: 800 },
		colorScheme: theme,
		reducedMotion: "no-preference",
	});
	await context.addInitScript((value) => localStorage.setItem("starlight-theme", value), theme);
	await context.addInitScript(collect);
	return context;
}

async function visit(context: BrowserContext, path: string, theme: string, steps?: (page: Page) => Promise<void>) {
	const name = `${path} (${theme})`;
	const page = await context.newPage();
	const errors: string[] = [];
	page.on("console", (m) => {
		if (m.type() === "error") errors.push(m.text());
	});
	page.on("pageerror", (e) => errors.push(String(e)));
	const response = await page.goto(`${origin}${path}`, { waitUntil: "networkidle" });
	if (!response?.ok()) fail(name, `status ${response?.status()}`);
	try {
		await steps?.(page);
	} catch (error) {
		fail(name, String(error instanceof Error ? error.message : error).split("\n")[0]);
	}
	const violations = await page.evaluate(() => (window as Probed).__csp ?? []);
	for (const v of violations) fail(name, `violation ${v.directive} blocked ${v.blocked} from ${v.source}`);
	for (const e of errors) fail(name, `console error: ${e}`);
	return page;
}

async function search(page: Page) {
	await page.locator("[data-open-modal]:not([disabled])").first().click();
	const input = page.locator("dialog[open] input");
	await input.fill("restore");
	await page.locator("dialog[open] .pagefind-ui__result-link, dialog[open] a[href*='/docs/']").first().waitFor({ timeout: 15000 });
}

async function clip(page: Page, theme: string) {
	const video = page.locator(`video.only-${theme}`).first();
	await video.scrollIntoViewIfNeeded();
	await page.waitForFunction(
		(sel) => {
			const v = document.querySelector<HTMLVideoElement>(sel);
			return !!v && !v.paused && v.currentTime > 0;
		},
		`video.only-${theme}`,
		{ timeout: 15000 },
	);
}

async function blockingProbe(browser: Browser, context: BrowserContext) {
	const bare = await browser.newContext({ ignoreHTTPSErrors: true });
	const blank = await bare.newPage();
	await blank.setContent("<!doctype html><body></body>");
	await blank.evaluate(probe);
	const control = await blank.evaluate(() => [(window as Probed).__cspProbe1, (window as Probed).__cspProbe2]);
	await bare.close();
	if (control[0] !== 1 || control[1] !== 1) {
		fail("probe control", `the probe did not run without a policy (${control.join(", ")})`);
		return;
	}
	const page = await context.newPage();
	await page.goto(`${origin}/`, { waitUntil: "networkidle" });
	await page.evaluate(() => {
		(window as Probed).__csp = [];
	});
	await page.evaluate(probe);
	await page.waitForTimeout(300);
	const flags = await page.evaluate(() => [(window as Probed).__cspProbe1, (window as Probed).__cspProbe2]);
	const events = await page.evaluate(() => (window as Probed).__csp ?? []);
	if (flags[0] !== undefined) fail("/", "an inline event handler ran under the policy");
	if (flags[1] !== undefined) fail("/", "an inline script ran under the policy");
	for (const directive of ["script-src-elem", "script-src-attr"]) {
		if (!events.some((e) => e.directive.startsWith(directive) && e.blocked === "inline")) {
			fail("/", `no ${directive} violation for the probe (got ${events.map((e) => `${e.directive}:${e.blocked}`).join(", ") || "none"})`);
		}
	}
}

const browser = await chromium.launch({ args: ["--host-resolver-rules=MAP lodestore.dev 127.0.0.1"] });
for (const theme of themes) {
	const context = await newContext(browser, theme);
	await visit(context, "/", theme);
	await visit(context, "/docs/quickstart/", theme, search);
	await visit(context, "/blog/introducing-lodestore/", theme, (page) => clip(page, theme));
	await visit(context, "/compare/restic/", theme);
	await visit(context, "/404.html", theme);
	if (theme === "light") await blockingProbe(browser, context);
	await context.close();
}
await browser.close();

if (failures.length > 0) {
	for (const f of failures) console.error(f);
	process.exit(1);
}
console.log("csp check ok");
