import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { toast } from "sonner";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	CapabilityChecklist,
	type CheckRow,
	toCheckRows,
} from "@/components/capability-checklist";
import { CodeBlock } from "@/components/code-block";
import { LabelsEditor } from "@/components/labels-editor";
import { PolicyPanel } from "@/components/policy-panel";
import { RelativeTime } from "@/components/relative-time";
import { StatusBadge } from "@/components/status-badge";
import { Stepper } from "@/components/stepper";
import { copyText } from "@/lib/clipboard";
import { type LabelRow, validateLabels } from "@/lib/labels";
import { expiresInDays, hostCount, onlineAgents } from "@/lib/plural";

describe("CapabilityChecklist", () => {
	it("always renders five rows in the fixed order inside a polite live region", () => {
		const { container } = render(<CapabilityChecklist rows={[]} />);
		expect(container.querySelector('[aria-live="polite"]')).not.toBeNull();
		const titles = screen
			.getAllByRole("listitem")
			.map((row) => row.querySelector('[data-slot="item-title"]')?.textContent);
		expect(titles).toEqual([
			"Write objects",
			"List objects",
			"Conditional write (If-None-Match)",
			"Agent key permissions",
			"Object Lock",
		]);
	});

	it("keeps the fixed order when rows arrive shuffled", () => {
		const rows: CheckRow[] = [
			{ id: "object_lock", status: "neutral", detail: "n/a" },
			{ id: "write", status: "pass", detail: "ok" },
		];
		render(<CapabilityChecklist rows={rows} />);
		const items = screen.getAllByRole("listitem");
		expect(items).toHaveLength(5);
		expect(within(items[0]).getByText("ok")).toBeInTheDocument();
		expect(within(items[4]).getByText("n/a")).toBeInTheDocument();
	});

	it("shows Checking… for running rows", () => {
		render(
			<CapabilityChecklist
				rows={[
					{ id: "write", status: "running" },
					{ id: "list", status: "running" },
					{ id: "conditional_write", status: "running" },
					{ id: "agent_delete", status: "running" },
					{ id: "object_lock", status: "running" },
				]}
			/>,
		);
		expect(screen.getAllByText("Checking…")).toHaveLength(5);
	});
});

describe("StatusBadge", () => {
	it("prints the status text next to the icon", () => {
		const { container } = render(<StatusBadge status="revoked" />);
		const badge = screen.getByText("Revoked").closest("[data-status]");
		expect(badge).toHaveAttribute("data-status", "revoked");
		expect(container.querySelector("svg")).not.toBeNull();
	});
});

describe("Stepper", () => {
	const steps = [
		{ title: "Storage", description: "Choose where backups live" },
		{ title: "Access", description: "Give Lodestore two keys" },
		{ title: "Recovery key", description: "Save the way back in" },
	];

	it("marks only the current step with aria-current", () => {
		render(<Stepper steps={steps} current={1} />);
		const current = screen
			.getAllByRole("listitem")
			.filter((li) => li.hasAttribute("aria-current"));
		expect(current).toHaveLength(1);
		expect(current[0]).toHaveTextContent("Access");
		expect(current[0]).toHaveAttribute("aria-current", "step");
	});

	it("shows step descriptions and names completed steps", () => {
		render(<Stepper steps={steps} current={1} />);
		expect(screen.getByText("Save the way back in")).toBeInTheDocument();
		const [done, now, next] = screen.getAllByRole("listitem");
		expect(done).toHaveAttribute("data-state", "completed");
		expect(done).toHaveTextContent("completed");
		expect(now).toHaveAttribute("data-state", "current");
		expect(next).toHaveAttribute("data-state", "upcoming");
		expect(next).not.toHaveTextContent("completed");
		expect(screen.getByRole("list", { name: "Progress" })).toBeInTheDocument();
	});
});

describe("CodeBlock", () => {
	it("is a focusable labelled region and copies its text", async () => {
		const user = userEvent.setup();
		const writeText = vi.spyOn(navigator.clipboard, "writeText");
		render(<CodeBlock text="curl -fsSL x | sh" object="install command" />);
		const region = screen.getByRole("region", { name: "install command" });
		expect(region).toHaveAttribute("tabindex", "0");
		await user.click(
			screen.getByRole("button", { name: "Copy install command" }),
		);
		expect(writeText).toHaveBeenCalledWith("curl -fsSL x | sh");
		expect(await screen.findByText("Copied")).toBeInTheDocument();
	});

	it("offers a line break after each slash without changing the text", () => {
		const text =
			"curl -fsSL http://192.0.2.10:8080/install.sh | sh -s -- --insecure --join lode1.abc";
		render(<CodeBlock text={text} object="install command" />);
		const region = screen.getByRole("region", { name: "install command" });
		expect(region.textContent).toBe(text);
		const breaks = region.querySelectorAll("wbr");
		expect(breaks.length).toBeGreaterThan(0);
		for (const br of breaks) {
			expect(br.previousElementSibling?.textContent).toMatch(/\/$/);
		}
	});

	it("adds no line break offers to text without a slash", () => {
		render(<CodeBlock text="lode1.abcdef" object="join token" />);
		expect(
			screen
				.getByRole("region", { name: "join token" })
				.querySelectorAll("wbr"),
		).toHaveLength(0);
	});

	it("disables the copy button when disabled", () => {
		render(<CodeBlock text="x" object="install command" disabled />);
		expect(
			screen.getByRole("button", { name: "Copy install command" }),
		).toBeDisabled();
	});
});

describe("copyText", () => {
	const clipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");

	afterEach(() => {
		vi.restoreAllMocks();
		if (clipboard) Object.defineProperty(navigator, "clipboard", clipboard);
		else Reflect.deleteProperty(navigator, "clipboard");
		Reflect.deleteProperty(document, "execCommand");
		window.getSelection()?.removeAllRanges();
	});

	function setClipboard(value: unknown) {
		Object.defineProperty(navigator, "clipboard", {
			value,
			configurable: true,
		});
	}

	function setExecCommand(result: boolean) {
		const exec = vi.fn(() => result);
		Object.defineProperty(document, "execCommand", {
			value: exec,
			configurable: true,
		});
		return exec;
	}

	it("uses the Clipboard API when it is available", async () => {
		const writeText = vi.fn(async () => {});
		setClipboard({ writeText });
		const exec = setExecCommand(true);

		expect(await copyText("secret")).toBe(true);
		expect(writeText).toHaveBeenCalledWith("secret");
		expect(exec).not.toHaveBeenCalled();
	});

	it("falls back to execCommand without the Clipboard API and restores focus", async () => {
		setClipboard(undefined);
		const exec = setExecCommand(true);
		render(<button type="button">anchor</button>);
		const anchor = screen.getByRole("button", { name: "anchor" });
		anchor.focus();

		expect(await copyText("secret")).toBe(true);
		expect(exec).toHaveBeenCalledWith("copy");
		expect(document.querySelector("textarea")).toBeNull();
		expect(anchor).toHaveFocus();
	});

	it("copies from a field next to the source inside a dialog", async () => {
		setClipboard(undefined);
		const exec = setExecCommand(true);
		render(
			<div role="dialog">
				<pre data-testid="src">secret</pre>
			</div>,
		);
		const source = screen.getByTestId("src");
		let selected = "";
		exec.mockImplementation(() => {
			const field = document.activeElement as HTMLTextAreaElement;
			selected = field.value.slice(field.selectionStart, field.selectionEnd);
			expect(field.parentElement).toBe(source.parentElement);
			return true;
		});

		expect(await copyText("secret", source)).toBe(true);
		expect(selected).toBe("secret");
		expect(document.querySelector("textarea")).toBeNull();
	});

	it("asks to select the text by hand when there is no source", async () => {
		setClipboard(undefined);
		setExecCommand(false);
		const error = vi.spyOn(toast, "error").mockImplementation(() => "id");

		expect(await copyText("secret")).toBe(false);
		expect(error).toHaveBeenCalledWith(
			"Could not copy automatically. Select the text and press Ctrl+C (Cmd+C on a Mac) to copy it.",
		);
	});

	it("selects the source and shows a toast when every method fails", async () => {
		setClipboard({
			writeText: vi.fn(async () => {
				throw new Error("denied");
			}),
		});
		setExecCommand(false);
		const error = vi.spyOn(toast, "error").mockImplementation(() => "id");
		render(<pre data-testid="src">secret</pre>);

		expect(await copyText("secret", screen.getByTestId("src"))).toBe(false);
		expect(error).toHaveBeenCalledWith(
			"Could not copy automatically. The text is selected: press Ctrl+C (Cmd+C on a Mac) to copy it.",
		);
		expect(window.getSelection()?.toString()).toBe("secret");
	});

	it("keeps the CodeBlock button unchanged when copying fails", async () => {
		const user = userEvent.setup();
		setClipboard(undefined);
		const error = vi.spyOn(toast, "error").mockImplementation(() => "id");
		render(<CodeBlock text="curl x" object="install command" />);

		await user.click(
			screen.getByRole("button", { name: "Copy install command" }),
		);
		expect(error).toHaveBeenCalled();
		expect(screen.queryByText("Copied")).not.toBeInTheDocument();
	});
});

describe("RelativeTime", () => {
	it("renders a time element with the ISO value and relative text", () => {
		const now = new Date("2026-10-14T12:00:00Z");
		render(<RelativeTime value="2026-10-14T11:55:00Z" now={now} />);
		const el = screen.getByText("5 minutes ago");
		expect(el.tagName).toBe("TIME");
		expect(el).toHaveAttribute("datetime", "2026-10-14T11:55:00.000Z");
	});
});

describe("validateLabels", () => {
	const row = (key: string, value = ""): LabelRow => ({ key, value });

	it("accepts ASCII keys and Unicode values", () => {
		expect(
			validateLabels([row("env", "prod"), row("team", "Платформа")]).valid,
		).toBe(true);
	});

	it("rejects keys with spaces or non-ASCII letters", () => {
		expect(validateLabels([row("bad key")]).rowErrors[0]?.field).toBe("key");
		expect(validateLabels([row("ключ")]).rowErrors[0]?.field).toBe("key");
	});

	it("requires a key when a value is set, and ignores fully blank rows", () => {
		expect(validateLabels([row("", "x")]).valid).toBe(false);
		expect(validateLabels([row("", "")]).valid).toBe(true);
	});

	it("rejects duplicate keys, long values and more than 20 labels", () => {
		expect(validateLabels([row("a"), row("a")]).valid).toBe(false);
		expect(validateLabels([row("a", "x".repeat(64))]).rowErrors[0]?.field).toBe(
			"value",
		);
		const many = Array.from({ length: 21 }, (_, i) => row(`k${i}`));
		expect(validateLabels(many).formError).not.toBeNull();
	});
});

function Harness({ initial = [] }: { initial?: LabelRow[] }) {
	const [rows, setRows] = useState<LabelRow[]>(initial);
	return <LabelsEditor value={rows} onChange={setRows} />;
}

describe("LabelsEditor", () => {
	it("shows one blank row with focus in Key when empty", () => {
		render(<Harness />);
		expect(screen.getByRole("textbox", { name: "Key" })).toHaveFocus();
	});

	it("shows the rule for a key with a space and links it with aria-describedby", async () => {
		const user = userEvent.setup();
		render(<Harness />);
		const key = screen.getByRole("textbox", { name: "Key" });
		await user.type(key, "bad key");
		const message = screen.getByText(/Keys and values: up to 63 characters/);
		expect(key).toHaveAttribute("aria-invalid", "true");
		expect(key.getAttribute("aria-describedby")).toBe(message.id);
	});

	it("accepts a Unicode value and rejects a Unicode key", async () => {
		const user = userEvent.setup();
		render(<Harness />);
		const key = screen.getByRole("textbox", { name: "Key" });
		const value = screen.getByRole("textbox", { name: "Value" });
		await user.type(key, "team");
		await user.type(value, "Платформа");
		expect(screen.queryByText(/Keys and values/)).toBeNull();
		await user.clear(key);
		await user.type(key, "ключ");
		expect(screen.getByText(/Keys and values/)).toBeInTheDocument();
	});

	it("does not allow a 21st row", () => {
		const twenty = Array.from({ length: 20 }, (_, i) => ({
			key: `k${i}`,
			value: "",
		}));
		render(<Harness initial={twenty} />);
		expect(screen.getByRole("button", { name: "Add label" })).toBeDisabled();
	});

	it("removes a row", async () => {
		const user = userEvent.setup();
		render(
			<Harness
				initial={[
					{ key: "a", value: "" },
					{ key: "b", value: "" },
				]}
			/>,
		);
		await user.click(
			screen.getAllByRole("button", { name: "Remove label" })[0],
		);
		expect(screen.getAllByRole("textbox", { name: "Key" })).toHaveLength(1);
	});
});

describe("plural", () => {
	it("handles one and many", () => {
		expect(onlineAgents(1)).toBe("1 online agent");
		expect(onlineAgents(5)).toBe("5 online agents");
		expect(expiresInDays(1)).toBe("Expires in 1 day");
		expect(expiresInDays(5)).toBe("Expires in 5 days");
		expect(hostCount(1)).toBe("1 host");
		expect(hostCount(5)).toBe("5 hosts");
	});
});

describe("PolicyPanel", () => {
	it("switches tabs and shows the MinIO commands", async () => {
		const user = userEvent.setup();
		render(
			<PolicyPanel
				iamJson='{"Version":"2012-10-17"}'
				minioCommands="mc admin policy create local lode-agent policy.json"
			/>,
		);
		expect(screen.getByText("Policy for the agent key")).toBeInTheDocument();
		expect(screen.getByText('{"Version":"2012-10-17"}')).toBeInTheDocument();
		await user.click(screen.getByRole("tab", { name: "MinIO" }));
		expect(screen.getByText(/mc admin policy/)).toBeInTheDocument();
	});
});

describe("toCheckRows", () => {
	const detail = (
		id: string,
		status: string,
		error?: string,
		agent_key_state = "unchecked",
	) => {
		const rows = toCheckRows(
			{ checks: [{ id, status, error }], agent_key_state },
			"s3",
		);
		render(<div>{rows[0].detail}</div>);
	};

	it("says Object Lock is not supported when the storage lacks the call", () => {
		detail("object_lock", "neutral", "not_supported");
		expect(
			screen.getByText("This storage does not support Object Lock."),
		).toBeInTheDocument();
	});

	it("shows the conditional write reason unless the backend lacks support", () => {
		detail(
			"conditional_write",
			"warn",
			"the storage is temporarily unavailable",
		);
		expect(
			screen.getByText("The storage is temporarily unavailable"),
		).toBeInTheDocument();
		detail("conditional_write", "warn", "unsupported");
		expect(screen.getByText(/^Not supported\./)).toBeInTheDocument();
	});
});
