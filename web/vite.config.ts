import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const target = "http://127.0.0.1:8080";

export default defineConfig({
	plugins: [
		tanstackRouter({
			target: "react",
			autoCodeSplitting: true,
			routeFileIgnorePattern: "\\.test\\.",
		}),
		react(),
		tailwindcss(),
	],
	resolve: {
		alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
	},
	server: {
		proxy: {
			"/api": target,
			"/install.sh": target,
			"/install": target,
			"/healthz": target,
		},
	},
	test: {
		environment: "jsdom",
		setupFiles: ["src/test/setup.ts"],
		testTimeout: 15_000,
	},
});
