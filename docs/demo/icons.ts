import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const mark = readFileSync(join(here, "..", "src", "assets", "mark.svg"), "utf8");
const pub = join(here, "..", "public");
const light = "#f5f7fb";

const browser = await chromium.launch();
const context = await browser.newContext({ colorScheme: "light", deviceScaleFactor: 1 });
const page = await context.newPage();

async function render(size: number, background: string | null, padding: number): Promise<Buffer> {
	await page.setViewportSize({ width: size, height: size });
	const inner = Math.round(size * (1 - padding * 2));
	await page.setContent(
		`<style>html,body{margin:0;height:100%;background:${background ?? "transparent"};display:grid;place-items:center}svg{width:${inner}px;height:${inner}px}</style>${mark}`,
	);
	return page.screenshot({ type: "png", omitBackground: background === null });
}

writeFileSync(join(pub, "apple-touch-icon.png"), await render(180, light, 0.12));
writeFileSync(join(pub, "icon-192.png"), await render(192, null, 0.04));
writeFileSync(join(pub, "icon-512.png"), await render(512, null, 0.04));

const png = await render(32, null, 0);
const header = Buffer.alloc(22);
header.writeUInt16LE(0, 0);
header.writeUInt16LE(1, 2);
header.writeUInt16LE(1, 4);
header.writeUInt8(32, 6);
header.writeUInt8(32, 7);
header.writeUInt16LE(1, 10);
header.writeUInt16LE(32, 12);
header.writeUInt32LE(png.length, 14);
header.writeUInt32LE(22, 18);
writeFileSync(join(pub, "favicon.ico"), Buffer.concat([header, png]));

await browser.close();
