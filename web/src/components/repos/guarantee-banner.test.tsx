import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { renderApp, repository, useSignedIn } from "@/test/app";
import { server } from "@/test/server";

const base = {
	...repository,
	location: { backend: "s3", options: { bucket: "my-bucket" } },
	checks: [],
	guarantee_notes: [] as string[],
};

function open(detail: Record<string, unknown>) {
	let current = { ...base, ...detail };
	let accepts = 0;
	useSignedIn({ repositories: [current] });
	server.use(
		http.get("/api/v1/repositories/repo-1", () => HttpResponse.json(current)),
		http.get("/api/v1/agent-policy", () =>
			HttpResponse.json({ iam_json: "{}", minio_commands: "mc" }),
		),
		http.post("/api/v1/repositories/repo-1/guarantee/accept", () => {
			accepts += 1;
			current = {
				...current,
				guarantee_accepted: current.guarantee_level as string,
			};
			return HttpResponse.json({
				guarantee_level: current.guarantee_level,
				guarantee_accepted: current.guarantee_accepted,
			});
		}),
	);
	renderApp("/settings/repositories/repo-1");
	return { accepts: () => accepts };
}

const weak = {
	guarantee_level: "host_can_overwrite_or_delete",
	guarantee_accepted: "host_can_overwrite_or_delete",
	guarantee_notes: ["agent_can_delete"],
};

describe("guarantee banner", () => {
	it("keeps showing what a host can do at an accepted weak level, without an action", async () => {
		open(weak);

		expect(
			await screen.findByText("A host could change or delete backups here"),
		).toBeInTheDocument();
		expect(screen.getByText("A host can delete backups.")).toBeInTheDocument();
		expect(
			screen.queryByRole("button", { name: "Accept the new level" }),
		).toBeNull();
	});

	it("shows a downgrade and accepts the new level", async () => {
		const { accepts } = open({
			...weak,
			guarantee_accepted: "overwrite_keeps_old_version",
		});
		const user = userEvent.setup();

		expect(
			await screen.findByText(
				"This storage protects backups less than you accepted",
			),
		).toBeInTheDocument();
		expect(
			screen.getByText(
				"Accepted: Overwrite keeps old version. Measured by the last check: Host can overwrite or delete.",
			),
		).toBeInTheDocument();

		await user.click(
			screen.getByRole("button", { name: "Accept the new level" }),
		);
		await waitFor(() => expect(accepts()).toBe(1));
		await waitFor(() =>
			expect(
				screen.queryByText(
					"This storage protects backups less than you accepted",
				),
			).toBeNull(),
		);
		expect(
			screen.getByText("A host could change or delete backups here"),
		).toBeInTheDocument();
	});

	it("asks to accept a measured weak level after an unchecked one", async () => {
		open({ ...weak, guarantee_accepted: "unchecked" });

		expect(
			await screen.findByRole("button", { name: "Accept the new level" }),
		).toBeInTheDocument();
	});

	it("shows an unchecked level with a link to the checks and no accept action", async () => {
		open({
			guarantee_level: "unchecked",
			guarantee_accepted: "unchecked",
		});

		expect(
			await screen.findByText("Lodestore could not measure what a host can do"),
		).toBeInTheDocument();
		expect(
			screen.getByRole("link", { name: "See storage checks" }),
		).toHaveAttribute("href", "/settings/repositories/repo-1#storage-checks");
		expect(
			screen.queryByRole("button", { name: "Accept the new level" }),
		).toBeNull();
	});

	it("shows nothing for a strong level without notes", async () => {
		open({ guarantee_level: "overwrite_keeps_old_version" });

		await screen.findByText("Storage checks");
		expect(screen.queryByText(/Notes on this storage/)).toBeNull();
		expect(screen.queryByText(/could change or delete/)).toBeNull();
	});

	it("lists a note on a create-only level as information, without accept", async () => {
		open({
			guarantee_level: "create_only_enforced",
			guarantee_accepted: "create_only_enforced",
			guarantee_notes: ["lifecycle_unverified"],
		});

		expect(
			await screen.findByText("Notes on this storage"),
		).toBeInTheDocument();
		expect(
			screen.getByText(/lifecycle rules could not be read/),
		).toBeInTheDocument();
		expect(
			screen.queryByRole("button", { name: "Accept the new level" }),
		).toBeNull();
	});
});
