import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { type BrowserContext, chromium, type Locator, type Page } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, ".out");
const media = join(here, "..", "src", "assets", "media");

const env = Object.fromEntries(
	readFileSync(join(out, "stand.env"), "utf8")
		.split("\n")
		.filter((line) => line.includes("="))
		.map((line) => [line.slice(0, line.indexOf("=")), line.slice(line.indexOf("=") + 1)]),
);
const base = `http://127.0.0.1:${env.DEMO_PORT}`;
const themes = ["light", "dark"] as const;
type Theme = (typeof themes)[number];

mkdirSync(join(out, "shots"), { recursive: true });
mkdirSync(media, { recursive: true });

async function newContext(
	browser: Awaited<ReturnType<typeof chromium.launch>>,
	theme: Theme,
	videoDir?: string,
	storageState?: Awaited<ReturnType<BrowserContext["storageState"]>>,
): Promise<BrowserContext> {
	const context = await browser.newContext({
		viewport: { width: 1280, height: 800 },
		deviceScaleFactor: videoDir ? 1 : 2,
		colorScheme: theme,
		...(storageState ? { storageState } : {}),
		...(videoDir ? { recordVideo: { dir: videoDir, size: { width: 1280, height: 800 } } } : {}),
	});
	await context.addInitScript((value) => localStorage.setItem("lodestore.theme", value), theme);
	return context;
}

const cursorScript = () => {
	const draw = () => {
		const cursor = document.createElement("div");
		cursor.style.cssText =
			"position:fixed;left:0;top:0;width:22px;height:22px;z-index:2147483647;pointer-events:none;transform:translate(-100px,-100px);transition:none;";
		cursor.innerHTML =
			'<svg width="22" height="22" viewBox="0 0 22 22" xmlns="http://www.w3.org/2000/svg"><path d="M3 2l14 7.5-6 1.8-2.2 6z" fill="#111" stroke="#fff" stroke-width="1.5" stroke-linejoin="round"/></svg>';
		document.documentElement.append(cursor);
		addEventListener(
			"mousemove",
			(event) => {
				cursor.style.transform = `translate(${event.clientX}px,${event.clientY}px)`;
			},
			true,
		);
	};
	if (document.documentElement) draw();
	else addEventListener("DOMContentLoaded", draw);
};

async function glide(page: Page, target: Locator) {
	await target.scrollIntoViewIfNeeded();
	const box = await target.boundingBox();
	if (!box) throw new Error("the target has no box to move to");
	const to = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
	const from = pointer.get(page) ?? { x: 640, y: 400 };
	const steps = 24;
	for (let i = 1; i <= steps; i++) {
		const t = i / steps;
		const eased = t * t * (3 - 2 * t);
		await page.mouse.move(from.x + (to.x - from.x) * eased, from.y + (to.y - from.y) * eased);
		await page.waitForTimeout(30);
	}
	pointer.set(page, to);
	await page.waitForTimeout(250);
}

const pointer = new WeakMap<Page, { x: number; y: number }>();

async function pointAndClick(page: Page, target: Locator) {
	await glide(page, target);
	await target.click();
}

async function login(page: Page) {
	await page.goto(`${base}/login`);
	await page.locator("#username").fill(env.DEMO_ADMIN_USERNAME);
	await page.locator("#password").fill(env.DEMO_ADMIN_PASSWORD);
	await page.locator("button[type=submit]").click();
	await page.waitForURL((url) => !url.pathname.startsWith("/login"));
}

async function settle(page: Page) {
	await page.waitForLoadState("networkidle");
	await page.waitForTimeout(800);
}

async function shot(page: Page, name: string, theme: Theme) {
	const png = join(out, "shots", `${name}.${theme}.png`);
	await page.screenshot({ path: png });
	execFileSync("cwebp", [
		"-quiet",
		"-q",
		"82",
		"-resize",
		"1600",
		"0",
		png,
		"-o",
		join(media, `${name}.${theme}.webp`),
	]);
}

async function fleet() {
	const browser = await chromium.launch();
	for (const theme of themes) {
		const context = await newContext(browser, theme);
		const page = await context.newPage();
		await login(page);
		await page.goto(`${base}/overview`);
		await page.getByText(/Stored [\d.]+ \w+iB/).waitFor();
		await settle(page);
		await shot(page, "overview", theme);

		await page.goto(`${base}/policies/new`);
		await page.getByPlaceholder("Key").fill("demo");
		await page.getByPlaceholder("Value").fill("agent");
		await page.getByText("matchLabels:").waitFor();
		await settle(page);
		await shot(page, "policy", theme);

		const hosts = await (await context.request.get(`${base}/api/v1/hosts`)).json();
		const failing = hosts.find((host: { hostname: string }) => host.hostname === "demo-db-01");
		await page.goto(`${base}/hosts/${failing.id}`);
		await page.getByText("Alerts for this host").waitFor();
		await settle(page);
		await shot(page, "alerts", theme);

		await page.goto(`${base}/restore`);
		await page.getByRole("tab", { name: "Find file" }).click();
		const query = page.getByPlaceholder(/Name or glob/);
		await query.fill("handbook");
		await query.press("Enter");
		await page.getByText(/Done · searched/).first().waitFor();
		await settle(page);
		await shot(page, "search", theme);
		await context.close();
	}
	await browser.close();
}

type Span = [number, number];

const IDLE_SPEED = 4;
const LEAD_IN = 0.6;

function speedFilter(idle: Span[]) {
	const cuts = [LEAD_IN, ...idle.flat()];
	const parts = cuts.map((start, i) => {
		const end = cuts[i + 1];
		const speed = i % 2 === 1 ? IDLE_SPEED : 1;
		const range = end === undefined ? `start=${start}` : `start=${start}:end=${end}`;
		return `[0:v]trim=${range},setpts=(PTS-STARTPTS)/${speed}[v${i}]`;
	});
	const labels = cuts.map((_, i) => `[v${i}]`).join("");
	return `${parts.join(";")};${labels}concat=n=${cuts.length}:v=1:a=0[out]`;
}

function encodeClip(clip: string, theme: Theme, webm: string, idle: Span[]) {
	const target = {
		mp4: join(media, `${clip}.${theme}.mp4`),
		poster: join(media, `${clip}-poster.${theme}.webp`),
	};
	execFileSync("ffmpeg", [
		"-y",
		"-v",
		"error",
		"-i",
		webm,
		"-filter_complex",
		speedFilter(idle),
		"-map",
		"[out]",
		"-an",
		"-c:v",
		"libx264",
		"-pix_fmt",
		"yuv420p",
		"-crf",
		"30",
		"-r",
		"15",
		"-movflags",
		"+faststart",
		target.mp4,
	]);
	const frame = join(out, "shots", `${clip}-poster.${theme}.png`);
	execFileSync("ffmpeg", ["-y", "-v", "error", "-sseof", "-0.1", "-i", target.mp4, "-frames:v", "1", frame]);
	execFileSync("cwebp", ["-quiet", "-q", "80", "-resize", "1280", "0", frame, "-o", target.poster]);
}

async function recordClip(
	browser: Awaited<ReturnType<typeof chromium.launch>>,
	theme: Theme,
	clip: string,
	scenario: (page: Page, idle: (wait: () => Promise<unknown>) => Promise<void>) => Promise<void>,
) {
	const signIn = await newContext(browser, theme);
	await login(await signIn.newPage());
	const session = await signIn.storageState();
	await signIn.close();

	const dir = join(out, "video", `${clip}-${theme}`);
	const context = await newContext(browser, theme, dir, session);
	await context.addInitScript(cursorScript);
	const spans: Span[] = [];
	const started = Date.now();
	const page = await context.newPage();
	const video = page.video();
	const idle = async (wait: () => Promise<unknown>) => {
		const from = (Date.now() - started) / 1000;
		await wait();
		spans.push([from, (Date.now() - started) / 1000]);
	};
	try {
		await scenario(page, idle);
		await page.waitForTimeout(1200);
	} finally {
		await context.close();
	}
	encodeClip(clip, theme, (await video?.path()) ?? "", spans);
}

async function restoreClip(page: Page) {
	await page.goto(`${base}/restore`);
	await page.getByRole("tab", { name: "Find file" }).click();
	await page.waitForTimeout(600);
	const query = page.getByPlaceholder(/Name or glob/);
	await pointAndClick(page, query);
	await query.pressSequentially("handbook", { delay: 90 });
	await query.press("Enter");
	await page.getByText(/Done · searched/).first().waitFor();
	await page.waitForTimeout(900);
	await pointAndClick(page, page.getByRole("button", { name: /^Restore/ }).first());
	const dialog = page.getByRole("dialog");
	await dialog.waitFor();
	await page.waitForTimeout(1200);
	const folder = dialog.getByLabel(/^Folder on/);
	await pointAndClick(page, folder);
	await folder.fill("");
	await folder.pressSequentially("/agent/restored", { delay: 70 });
	await page.waitForTimeout(700);
	await pointAndClick(page, dialog.getByRole("button", { name: "Start restore" }));
	const viewJob = page.getByRole("button", { name: "View job" }).or(page.getByRole("link", { name: "View job" }));
	await pointAndClick(page, viewJob.first());
	await page.getByText(/succeeded|Succeeded/).first().waitFor({ timeout: 90_000 });
	await page.waitForTimeout(800);
}

async function addHostClip(page: Page, idle: (wait: () => Promise<unknown>) => Promise<void>) {
	await page.goto(`${base}/hosts`);
	await page.waitForTimeout(500);
	await pointAndClick(page, page.getByRole("button", { name: "Add host" }).first());
	const dialog = page.getByRole("dialog");
	await dialog.waitFor();
	await page.waitForTimeout(300);
	const key = dialog.getByPlaceholder("Key");
	await pointAndClick(page, key);
	await key.pressSequentially("demo", { delay: 50 });
	await dialog.getByPlaceholder("Value").pressSequentially("agent", { delay: 50 });
	await pointAndClick(page, dialog.getByRole("button", { name: "Generate token" }));
	const command = dialog.locator("pre").first();
	await command.waitFor();
	const token = (await command.innerText()).match(/--join\s+(\S+)/)?.[1];
	if (!token || !/^[\w.~-]+$/.test(token)) throw new Error("no usable join token in the dialog");
	await page.waitForTimeout(300);
	await pointAndClick(page, dialog.getByRole("button", { name: "Copy install command" }));
	await page.waitForTimeout(500);
	execFileSync("bash", [standScript, "addhost-token", token], { stdio: "ignore" });
	await idle(() => dialog.getByText(/is online/).waitFor({ timeout: 90_000 }));
	await page.waitForTimeout(400);
	await pointAndClick(page, dialog.getByRole("button", { name: "Finish host setup" }));
	await page.waitForTimeout(400);
	await pointAndClick(page, page.getByRole("link", { name: "demo-app-01" }).first());
	await page.waitForTimeout(300);
	await pointAndClick(page, page.getByRole("button", { name: /Run backup/ }).first());
	await idle(() => page.getByText("Protected", { exact: true }).first().waitFor({ timeout: 90_000 }));
	await page.waitForTimeout(500);
}

async function clips() {
	const wanted = process.argv.slice(3);
	const chosen = themes.filter((theme) => wanted.length === 0 || wanted.includes(theme));
	const browser = await chromium.launch();
	for (const theme of chosen) {
		await recordClip(browser, theme, "restore", (page) => restoreClip(page));
		execFileSync("bash", [standScript, "addhost-start"], { stdio: "ignore" });
		await recordClip(browser, theme, "add-host", addHostClip);
	}
	await browser.close();
}

const standScript = join(here, "stand.sh");
const wizardBucket = "wizard";

function prepareWizardStorage(commands: string, accessKey: string, secretKey: string) {
	const filled = commands
		.replaceAll("ALIAS", "s")
		.replaceAll("AGENT_ACCESS_KEY", accessKey)
		.replaceAll("AGENT_SECRET_KEY", secretKey);
	const script = [
		"set -e",
		'mc alias set s http://minio:9000 "$MINIO_ROOT_USER" "$MINIO_ROOT_PASSWORD" >/dev/null',
		`mc mb --ignore-existing s/${wizardBucket} >/dev/null`,
		"cd /tmp",
		filled,
	].join("\n");
	execFileSync("bash", [standScript, "mc"], { input: script, stdio: ["pipe", "ignore", "inherit"] });
}

async function wizard() {
	const agentAccess = randomBytes(8).toString("hex");
	const agentSecret = randomBytes(16).toString("hex");
	const browser = await chromium.launch();
	for (const theme of themes) {
		const context = await newContext(browser, theme);
		const page = await context.newPage();
		await login(page);
		const policy = await context.request.get(`${base}/api/v1/agent-policy?bucket=${wizardBucket}`);
		prepareWizardStorage((await policy.json()).minio_commands, agentAccess, agentSecret);

		await page.goto(`${base}/welcome`);
		await page.locator("#repository-name").fill("office-backups");
		await page.locator("#backend-s3").check();
		await page.getByRole("button", { name: "Continue to access" }).click();
		await page.locator("#s3-endpoint").fill("http://minio:9000");
		await page.locator("#s3-bucket").fill(wizardBucket);
		await page.locator("#server-key-id").fill(env.MINIO_ROOT_USER);
		await page.locator("#server-key-secret").fill(env.MINIO_ROOT_PASSWORD);
		await page.locator("#agent-key-id").fill(agentAccess);
		await page.locator("#agent-key-secret").fill(agentSecret);
		await page.getByRole("button", { name: "Check storage" }).click();
		await page.getByRole("button", { name: "Continue to recovery key" }).waitFor();
		await page.getByText(/\d+ passed/).evaluate((el) => el.scrollIntoView({ block: "start" }));
		await page.evaluate(() => document.scrollingElement?.scrollBy(0, -130));
		await settle(page);
		await shot(page, "wizard", theme);
		await context.close();
	}
	await browser.close();
}

const sets: Record<string, () => Promise<void>> = { wizard, fleet, clips };
const wanted = process.argv[2] ?? "";
if (!(wanted in sets)) {
	console.error(`usage: capture.ts ${Object.keys(sets).join("|")}`);
	process.exit(2);
}
await sets[wanted]();
