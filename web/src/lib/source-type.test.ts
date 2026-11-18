import { describe, expect, it } from "vitest";
import { copy } from "@/copy";
import {
	parseSourceType,
	SOURCE_TYPES,
	sourceTypeIcon,
	sourceTypeLabel,
} from "@/lib/source-type";

describe("source types", () => {
	it("keeps the UI-SPEC order without WAL", () => {
		expect(SOURCE_TYPES).toEqual([
			"files",
			"proxmox-vm",
			"proxmox-ct",
			"postgres",
			"mysql",
		]);
	});

	it("has a label and an icon for every type", () => {
		for (const type of SOURCE_TYPES) {
			expect(sourceTypeLabel(type)).not.toBe("");
			expect(sourceTypeIcon[type]).toBeDefined();
		}
		expect(sourceTypeLabel("proxmox-vm")).toBe(copy.sources.type.proxmoxVm);
	});

	it("parses only known types", () => {
		expect(parseSourceType("proxmox-vm")).toBe("proxmox-vm");
		expect(parseSourceType("oracle")).toBeUndefined();
		expect(parseSourceType(undefined)).toBeUndefined();
		expect(parseSourceType(5)).toBeUndefined();
	});
});
