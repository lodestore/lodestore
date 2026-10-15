import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Compass } from "lucide-react";
import { describe, expect, it } from "vitest";
import { Illustration } from "@/components/brand/illustration";
import { PreviewPage } from "@/components/preview-page";
import { ThemeProvider, useTheme } from "@/components/theme/theme-provider";
import { copy } from "@/copy";

const { title, lead, bullets } = copy.preview.overview;

function ThemeButton() {
	const { setTheme } = useTheme();
	return (
		<button type="button" onClick={() => setTheme("dark")}>
			dark
		</button>
	);
}

function renderPreview(embedded = false) {
	return render(
		<ThemeProvider>
			<ThemeButton />
			<PreviewPage
				icon={Compass}
				title={title}
				lead={lead}
				bullets={bullets}
				illustration="preview-overview"
				embedded={embedded}
			/>
		</ThemeProvider>,
	);
}

const imgSrc = () => (document.querySelector("img") as HTMLImageElement).src;

describe("PreviewPage", () => {
	it("renders the full preview without controls", () => {
		renderPreview();
		const h1 = screen.getByRole("heading", { level: 1, name: title });
		expect(h1).toHaveAttribute("data-page-title");
		expect(h1).toHaveAttribute("tabindex", "-1");
		expect(screen.getByText(copy.preview.badge)).toBeInTheDocument();
		expect(screen.getByText(lead)).toBeInTheDocument();
		expect(screen.getAllByRole("listitem")).toHaveLength(3);
		expect(screen.getByText(copy.preview.footnote)).toBeInTheDocument();
		expect(screen.getAllByRole("button")).toHaveLength(1);
	});

	it("renders the embedded variant as h2 without list", () => {
		renderPreview(true);
		expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
		expect(
			screen.getByRole("heading", { level: 2, name: title }),
		).toBeVisible();
		expect(screen.queryByRole("list")).toBeNull();
		expect(screen.getByText(copy.preview.badge)).toBeInTheDocument();
		expect(screen.getByText(lead)).toBeInTheDocument();
	});

	it("swaps the single image with the theme", async () => {
		renderPreview();
		expect(document.querySelectorAll("img")).toHaveLength(1);
		expect(document.querySelector("img")).toHaveAttribute("alt", "");
		expect(imgSrc()).toContain("preview-overview.light");
		await userEvent.click(screen.getByRole("button", { name: "dark" }));
		expect(imgSrc()).toContain("preview-overview.dark");
	});

	it("renders nothing for a missing file", () => {
		const { container } = render(
			<ThemeProvider>
				<Illustration name={"nope" as never} width={10} height={10} />
			</ThemeProvider>,
		);
		expect(container).toBeEmptyDOMElement();
	});
});
