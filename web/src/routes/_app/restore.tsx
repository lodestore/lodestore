import { createFileRoute } from "@tanstack/react-router";
import { z } from "zod";
import { RestoreWorkspace } from "@/components/restore/restore-workspace";

const text = z
	.union([z.string(), z.number()])
	.transform(String)
	.optional()
	.catch(undefined);

const matching = (pattern: RegExp) =>
	text
		.refine((value) => value === undefined || pattern.test(value))
		.catch(undefined);

export const Route = createFileRoute("/_app/restore")({
	validateSearch: z.object({
		host: text,
		snapshot: matching(/^[0-9a-f]{64}$/),
		path: matching(/^(?:[A-Za-z0-9_-]{4})*(?:[A-Za-z0-9_-]{2,3})?$/),
		q: text,
	}),
	component: RestoreWorkspace,
});
