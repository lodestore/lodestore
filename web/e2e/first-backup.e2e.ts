import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import AxeBuilder from "@axe-core/playwright";
import { expect, type Locator, type Page, test } from "@playwright/test";
import { copy } from "../src/copy";
import { restore } from "../src/copy.restore";

function stand(name: string) {
	const value = process.env[name];
	if (!value) {
		throw new Error(
			`${name} is not set: run scripts/quickstart-ci.sh <image> --e2e`,
		);
	}
	return value;
}

async function a11y(page: Page, marker: Locator) {
	await expect(marker).toBeVisible();
	const results = await new AxeBuilder({ page })
		.withTags(["wcag2a", "wcag2aa"])
		.analyze();
	expect(results.violations).toEqual([]);
	expect(results.passes.length).toBeGreaterThan(0);
}

function prepareStorage(
	commands: string,
	accessKey: string,
	secretKey: string,
) {
	const filled = commands
		.replaceAll("ALIAS", stand("E2E_MC_ALIAS"))
		.replaceAll("AGENT_ACCESS_KEY", accessKey)
		.replaceAll("AGENT_SECRET_KEY", secretKey)
		.replace(/^mc /gm, `${stand("E2E_MC")} `);
	const cwd = mkdtempSync(join(tmpdir(), "e2e-agent-policy-"));
	try {
		execFileSync("bash", ["-c", filled], { cwd, stdio: "ignore" });
	} finally {
		rmSync(cwd, { recursive: true, force: true });
	}
}

test("setup, add host, policy, backup and restore through the UI", async ({
	page,
}) => {
	const adminPassword = randomBytes(16).toString("hex");
	const repositoryPassword = randomBytes(16).toString("hex");
	const agentAccess = randomBytes(8).toString("hex");
	const agentSecret = randomBytes(16).toString("hex");
	const wizard = copy.wizard;

	await test.step("setup", async () => {
		await page.goto(`/setup?token=${stand("E2E_SETUP_TOKEN")}`);
		await a11y(page, page.locator("#username"));
		await page.locator("#username").fill("admin");
		await page.locator("#password").fill(adminPassword);
		await page.locator("#confirm").fill(adminPassword);
		await page.getByRole("button", { name: copy.setup.button }).click();
		await page.waitForURL(/\/welcome/);
	});

	await test.step("repository: storage", async () => {
		await a11y(page, page.getByRole("heading", { name: "Choose storage" }));
		await page.locator("#repository-name").fill("e2e");
		await page.locator("#backend-s3").check();
		await page.getByRole("button", { name: wizard.continueToAccess }).click();
	});

	await test.step("repository: access", async () => {
		const policy = await page.request.get(
			`/api/v1/agent-policy?bucket=${stand("E2E_BUCKET")}`,
		);
		expect(policy.ok()).toBe(true);
		prepareStorage(
			(await policy.json()).minio_commands,
			agentAccess,
			agentSecret,
		);

		await page.locator("#s3-endpoint").fill(stand("E2E_S3_ENDPOINT"));
		await page.locator("#s3-bucket").fill(stand("E2E_BUCKET"));
		await page.locator("#server-key-id").fill(stand("E2E_MINIO_USER"));
		await page.locator("#server-key-secret").fill(stand("E2E_MINIO_PASSWORD"));
		await page.locator("#agent-key-id").fill(agentAccess);
		await page.locator("#agent-key-secret").fill(agentSecret);
		await a11y(page, page.getByRole("heading", { name: "Check access" }));
		await page.getByRole("button", { name: "Check storage" }).click();
		await page
			.getByRole("button", { name: wizard.continueToKey })
			.click({ timeout: 90_000 });
	});

	await test.step("repository: recovery key", async () => {
		const keyPanel = page.getByRole("region", { name: "recovery key" });
		await a11y(page, keyPanel);
		const key =
			(await keyPanel.locator(".sr-only").textContent())?.trim() ?? "";
		const lastGroup = key.split("-").at(-1);
		expect(lastGroup).toBeTruthy();
		await page.locator("#repository-password").fill(repositoryPassword);
		await page.locator("#repository-password-confirm").fill(repositoryPassword);
		await page.locator("#recovery-key-last-group").fill(lastGroup ?? "");
		await page.getByRole("button", { name: wizard.createRepository }).click();
	});

	await test.step("add host", async () => {
		await page.getByLabel(copy.labels.key, { exact: true }).fill("agent");
		await page.getByLabel(copy.labels.value, { exact: true }).fill("e2e");
		await page.getByRole("button", { name: copy.addHost.generate }).click();
		const command = page.getByRole("region", {
			name: copy.addHost.commandObject,
		});
		await a11y(page, command);
		const token = (await command.innerText()).match(/--join\s+(\S+)/)?.[1];
		if (!token || !/^[\w.~-]+$/.test(token)) {
			throw new Error("no usable join token in the add host panel");
		}
		execFileSync(stand("E2E_INSTALL_AGENT"), [token], { stdio: "inherit" });
		await expect(page.getByText(/is online/)).toBeVisible({ timeout: 120_000 });
	});

	await test.step("policy", async () => {
		await page.getByRole("link", { name: copy.firstHost.nextStep }).click();
		await page.waitForURL(/\/policies\/new/);
		await a11y(
			page,
			page.getByRole("textbox", { name: copy.policies.form.name, exact: true }),
		);
		await page
			.getByRole("textbox", { name: copy.policies.form.name, exact: true })
			.fill("e2e");
		await page.getByLabel(copy.labels.key, { exact: true }).fill("agent");
		await page.getByLabel(copy.labels.value, { exact: true }).fill("e2e");
		const paths = page.getByRole("textbox", {
			name: copy.policies.form.paths,
			exact: true,
		});
		await page
			.getByRole("button", { name: copy.policies.form.removePath })
			.nth(1)
			.click();
		await paths.first().fill(stand("E2E_DATA"));
		const save = page.getByRole("button", { name: copy.policies.save });
		await expect(save).toBeEnabled();
		await save.click();
		await page.waitForURL(/\/policies\/e2e$/);
	});

	await test.step("backup", async () => {
		await page.getByRole("button", { name: copy.policies.runNow }).click();
		await page
			.getByRole("button", { name: copy.policies.run.confirm(1) })
			.click();
		await page.getByRole("button", { name: copy.jobs.viewJob }).click();
		await a11y(page, page.getByRole("heading", { level: 1 }));
		await expect(
			page.getByText(copy.jobs.status.succeeded, { exact: true }).first(),
		).toBeVisible({
			timeout: 180_000,
		});
	});

	await test.step("restore", async () => {
		await page.goto("/restore");
		await a11y(
			page,
			page.getByRole("button", { name: restore.browser.openFolder("srv") }),
		);
		await page
			.getByRole("button", { name: restore.browser.openFolder("srv") })
			.click();
		await page
			.getByRole("button", {
				name: restore.browser.openFolder("lodestore-quickstart"),
			})
			.click();
		await page
			.getByRole("checkbox", { name: restore.selection.select("data") })
			.check();
		await page
			.getByRole("button", { name: /^Restore/ })
			.first()
			.click();
		const dialog = page.getByRole("dialog");
		await dialog.getByLabel(/^Folder on/).fill(stand("E2E_RESTORE_TO"));
		await dialog
			.getByRole("button", { name: restore.restoreDialog.confirm })
			.click();
		await page.getByRole("button", { name: restore.viewJob }).click();
		await expect(
			page.getByText(copy.jobs.status.succeeded, { exact: true }).first(),
		).toBeVisible({
			timeout: 180_000,
		});
	});
});
