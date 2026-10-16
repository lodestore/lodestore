import { useCallback, useRef, useState } from "react";
import { copy } from "@/copy";
import { absolutePath, pathParts } from "@/lib/names";

export interface SelectedItem {
	pathB64: string;
	display: string;
	isDir: boolean;
}

interface State {
	items: Map<string, SelectedItem>;
	notice: string;
}

export type Selection = ReturnType<typeof useSelection>;

/** The selected folder whose subtree already includes everything in `dir`. */
export function coveringFolder(
	items: Map<string, SelectedItem>,
	dir: Uint8Array[],
) {
	if (items.size === 0) return undefined;
	for (let depth = 0; depth <= dir.length; depth += 1) {
		const hit = items.get(absolutePath(dir.slice(0, depth)));
		if (hit?.isDir) return hit;
	}
	return undefined;
}

/** True when something below `dir` is selected, at any depth. */
export function selectedInside(
	items: Map<string, SelectedItem>,
	dir: Uint8Array[],
) {
	const folder = absolutePath(dir);
	for (const key of items.keys()) {
		const parts = pathParts(key);
		if (
			parts.length > dir.length &&
			absolutePath(parts.slice(0, dir.length)) === folder
		) {
			return true;
		}
	}
	return false;
}

/** Drops items that a selected ancestor folder already includes. */
function dropCovered(items: Map<string, SelectedItem>) {
	const dirs = new Set<string>();
	for (const item of items.values()) if (item.isDir) dirs.add(item.pathB64);
	if (dirs.size === 0) return;
	for (const key of Array.from(items.keys())) {
		const parts = pathParts(key);
		for (let depth = 0; depth < parts.length; depth += 1) {
			if (dirs.has(absolutePath(parts.slice(0, depth)))) {
				items.delete(key);
				break;
			}
		}
	}
}

export function useSelection() {
	const [state, setState] = useState<State>({
		items: new Map(),
		notice: "",
	});
	const anchor = useRef<string>(undefined);

	const change = useCallback(
		(edit: (items: Map<string, SelectedItem>) => void) =>
			setState((prev) => {
				const items = new Map(prev.items);
				edit(items);
				dropCovered(items);
				const emptied = items.size === 0 && prev.items.size > 0;
				return {
					items,
					notice: emptied ? copy.restore.selection.cleared : "",
				};
			}),
		[],
	);

	const selectFolder = useCallback(
		(item: SelectedItem) =>
			change((items) => {
				if (!items.delete(item.pathB64)) items.set(item.pathB64, item);
			}),
		[change],
	);

	const toggle = useCallback(
		(item: SelectedItem) => {
			anchor.current = item.pathB64;
			selectFolder(item);
		},
		[selectFolder],
	);

	const toggleRange = useCallback(
		(visible: SelectedItem[], item: SelectedItem) => {
			const from = visible.findIndex((v) => v.pathB64 === anchor.current);
			const to = visible.findIndex((v) => v.pathB64 === item.pathB64);
			if (from < 0 || to < 0) return toggle(item);
			anchor.current = item.pathB64;
			change((items) => {
				for (const v of visible.slice(
					Math.min(from, to),
					Math.max(from, to) + 1,
				)) {
					items.set(v.pathB64, v);
				}
			});
		},
		[change, toggle],
	);

	const clear = useCallback((notice?: string) => {
		anchor.current = undefined;
		setState((prev) =>
			prev.items.size === 0
				? prev
				: {
						items: new Map(),
						notice: notice ?? copy.restore.selection.cleared,
					},
		);
	}, []);

	const count = state.items.size;
	return {
		items: state.items,
		count,
		announcement:
			count > 0 ? copy.restore.selection.selected(count) : state.notice,
		toggle,
		toggleRange,
		selectFolder,
		clear,
	};
}
