import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { type BrowserContext, chromium, type Page } from "playwright";

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
): Promise<BrowserContext> {
	const context = await browser.newContext({
		viewport: { width: 1280, height: 800 },
		deviceScaleFactor: videoDir ? 1 : 2,
		colorScheme: theme,
		...(videoDir ? { recordVideo: { dir: videoDir, size: { width: 1280, height: 800 } } } : {}),
	});
	await context.addInitScript((value) => localStorage.setItem("lodestore.theme", value), theme);
	return context;
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
		await context.close();
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

const sets: Record<string, () => Promise<void>> = { wizard, fleet };
const wanted = process.argv[2] ?? "";
if (!(wanted in sets)) {
	console.error(`usage: capture.ts ${Object.keys(sets).join("|")}`);
	process.exit(2);
}
await sets[wanted]();
