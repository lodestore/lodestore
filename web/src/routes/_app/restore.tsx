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

const whole = z
	.union([z.number(), z.string()])
	.transform(Number)
	.pipe(z.number().int().min(0))
	.optional()
	.catch(undefined);

const BASE64URL = /^(?:[A-Za-z0-9_-]{4})*(?:[A-Za-z0-9_-]{2,3})?$/;

export const Route = createFileRoute("/_app/restore")({
	validateSearch: z.object({
		host: text,
		snapshot: matching(/^[0-9a-f]{64}$/),
		path: matching(BASE64URL),
		q: text,
		tab: z.enum(["host", "find"]).optional().catch(undefined),
		find: text,
		hosts: text,
		labels: text,
		since: whole,
		until: whole,
		run: whole,
		file: text,
		select: matching(BASE64URL),
	}),
	component: RestoreWorkspace,
});
