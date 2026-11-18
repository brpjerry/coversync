// What an album's numbering says it lacks, in words and as rows of its
// tracklist.

import type { Incomplete } from './api.ts';

/** "7", or "7–9" for a run. */
const span = (from: number, to: number) => (from === to ? `${from}` : `${from}–${to}`);

/** "a", "a and b", "a, b and c". */
function listed(items: string[]) {
	return items.length < 2
		? (items[0] ?? '')
		: `${items.slice(0, -1).join(', ')} and ${items.at(-1)}`;
}

/**
 * e.g. "Missing tracks 4 and 7–9", or on an album with several discs,
 * "Missing disc 1 track 4, disc 2 tracks 7–9 and disc 3".
 */
export function describeIncomplete(inc: Incomplete): string {
	// Gaps on no particular disc come first: an album with one, or numbered straight through.
	const parts: { disc: number; text: string }[] = [];
	const byDisc = new Map<number | null, { from: number; to: number }[]>();
	for (const g of inc.tracks) {
		if (!byDisc.has(g.disc)) byDisc.set(g.disc, []);
		byDisc.get(g.disc)!.push(g);
	}
	for (const [disc, gaps] of byDisc) {
		const count = gaps.reduce((n, g) => n + g.to - g.from + 1, 0);
		const numbers = `${count === 1 ? 'track' : 'tracks'} ${listed(gaps.map((g) => span(g.from, g.to)))}`;
		parts.push({ disc: disc ?? 0, text: disc === null ? numbers : `disc ${disc} ${numbers}` });
	}
	for (const d of inc.discs) parts.push({ disc: d, text: `disc ${d}` });
	// In disc order (gaps on no disc first, as 0).
	parts.sort((a, b) => a.disc - b.disc);
	const texts = parts.map((p) => p.text);
	if (inc.unplaced) texts.push(unplacedText(inc.unplaced, texts.length > 0));
	return `Missing ${listed(texts)}`;
}

/** Tracks an album-wide total asks for beyond the gaps: "2 more tracks whose disc the tags don't say". */
export function unplacedText(n: number, more: boolean) {
	return `${more ? `${n} more` : n} ${n === 1 ? 'track' : 'tracks'} whose disc the tags don't say`;
}

/** A row of an album's tracklist: one of its tracks, or a run of missing ones. */
export type Row<T> = { track: T } | { from: number; to: number };

/**
 * Which disc a gap with none of its own (numbered straight through the
 * discs) is on: the one whose tracks surround it, else the one missing disc
 * between those before and after it, else the one before it.
 */
function discOf<T extends { track_no: number | null }>(
	g: { from: number; to: number },
	discs: [number, T[]][],
	missing: number[]
): number {
	if (discs.length < 2) return discs[0]?.[0] ?? 1;
	const numbers = (ts: T[]) => ts.map((t) => t.track_no ?? 0);
	const around = discs.find(
		([, ts]) => numbers(ts).some((n) => n < g.from) && numbers(ts).some((n) => n > g.to)
	);
	if (around) return around[0];
	const before = discs.findLast(([, ts]) => numbers(ts).every((n) => n < g.from));
	const after = discs.find(([, ts]) => numbers(ts).every((n) => n > g.to));
	const between = missing.filter((d) => d > (before?.[0] ?? 0) && d < (after?.[0] ?? Infinity));
	if (between.length === 1) return between[0];
	return (before ?? after ?? discs[0])[0];
}

/**
 * The album's tracks, disc by disc as `discs` has them, with rows for the
 * runs of numbers it lacks where they belong, and missing discs as discs
 * whose only row is `null` (or the gap that's on them).
 */
export function withGaps<T extends { track_no: number | null }>(
	discs: [number, T[]][],
	inc: Incomplete | undefined
): [number, Row<T>[] | null][] {
	const out = new Map<number, Row<T>[] | null>(
		discs.map(([d, tracks]) => [d, tracks.map((track) => ({ track }))])
	);
	if (!inc) return [...out.entries()];
	for (const g of inc.tracks) {
		const disc = g.disc ?? discOf(g, discs, inc.discs);
		const rows = out.get(disc) ?? [];
		// Before the first track numbered after it.
		const at = rows.findIndex((r) => 'track' in r && (r.track.track_no ?? 0) > g.to);
		rows.splice(at === -1 ? rows.length : at, 0, { from: g.from, to: g.to });
		out.set(disc, rows);
	}
	for (const d of inc.discs) if (!out.has(d)) out.set(d, null);
	return [...out.entries()].sort(([a], [b]) => a - b);
}
