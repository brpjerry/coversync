// The album grid's layout, shared with the Appearance screen's cover size
// preview so it shows the cards exactly as the grid will.

/** The grid stops widening here and centres instead. */
export const GRID_MAX_WIDTH = 1800;
/** Below this grid width cards and gaps shrink to suit a phone. */
const NARROW = 560;
/** Phones show covers this much of the chosen size. */
const NARROW_SCALE = 0.8;

export interface GridLayout {
	narrow: boolean;
	colGap: number;
	rowGap: number;
	/** The chosen cover size as applied at this width. */
	minWidth: number;
	cols: number;
	/** Each card's width: the minimum, stretched so the row fills the grid. */
	cellWidth: number;
}

/** Lay out cards at least `size` px wide across a grid `width` px wide. */
export function gridLayout(width: number, size: number): GridLayout {
	const narrow = width < NARROW;
	const colGap = narrow ? 14 : 22;
	const rowGap = narrow ? 22 : 30;
	const minWidth = Math.round(narrow ? size * NARROW_SCALE : size);
	const cols = Math.max(1, Math.floor((width + colGap) / (minWidth + colGap)));
	const cellWidth = Math.max(0, (width - colGap * (cols - 1)) / cols);
	return { narrow, colGap, rowGap, minWidth, cols, cellWidth };
}
