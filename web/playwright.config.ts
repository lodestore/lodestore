import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
	testDir: "e2e",
	testMatch: "**/*.e2e.ts",
	timeout: 15 * 60_000,
	retries: 0,
	workers: 1,
	reporter: process.env.CI ? [["github"], ["html", { open: "never" }]] : "list",
	use: { baseURL: process.env.E2E_BASE_URL, trace: "retain-on-failure" },
	projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
