import { defineConfig } from "orval";

export default defineConfig({
	api: {
		input: "./openapi.json",
		output: {
			client: "react-query",
			httpClient: "axios",
			mode: "single",
			target: "src/api/gen/endpoints.ts",
			schemas: "src/api/gen/model",
			clean: true,
			override: {
				mutator: { path: "src/api/fetcher.ts", name: "apiFetch" },
			},
		},
	},
	zod: {
		input: "./openapi.json",
		output: {
			client: "zod",
			mode: "single",
			target: "src/api/gen/zod.ts",
		},
	},
});
