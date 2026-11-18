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

/**
 * Tracks an album-wide total asks for beyond the gaps, a minimum (the total
 * might be each disc's): "at least 2 more tracks whose disc the tags don't say".
 */
export function unplacedText(n: number, more: boolean) {
	return `at least ${n}${more ? ' more' : ''} ${n === 1 ? 'track' : 'tracks'} whose disc the tags don't say`;
}

/** A row of an album's tracklist: one of its tracks, or a run of missing ones. */
export type Row<T> = { track: T } | { from: number; to: number };

/** A disc of an album's tracklist, or a place between discs; `rows` null for a missing disc. */
export interface Group<T> {
	/** Orders the groups: the disc's number, or halfway past the disc before. */
	key: number;
	label: string;
	rows: Row<T>[] | null;
}

/** What placing a gap needs of a track. */
type Numbered = { track_no: number | null; disc_total?: number | null };

/**
 * Where a gap with no disc of its own (numbered straight through the discs)
 * goes: on the disc whose tracks surround it, on disc 1 before its tracks,
 * or on the last disc after its tracks when the disc total says it's last.
 * Otherwise the tags don't say whether it ends one disc, starts the next or
 * fills one between, so it goes between them.
 */
function placeOf<T extends Numbered>(
	g: { from: number; to: number },
	discs: [number, T[]][]
): { key: number; label: string } {
	const disc = (d: number) => ({ key: d, label: `Disc ${d}` });
	if (discs.length < 2) return disc(discs[0]?.[0] ?? 1);
	const numbers = (ts: T[]) => ts.map((t) => t.track_no ?? 0);
	const around = discs.find(
		([, ts]) => numbers(ts).some((n) => n < g.from) && numbers(ts).some((n) => n > g.to)
	);
	if (around) return disc(around[0]);
	const before = discs.findLast(([, ts]) => numbers(ts).every((n) => n < g.from))?.[0];
	const after = discs.find(([, ts]) => numbers(ts).every((n) => n > g.to))?.[0];
	if (before !== undefined && after !== undefined)
		return { key: before + 0.5, label: `Between discs ${before} and ${after}` };
	if (before !== undefined) {
		const declared = discs.flatMap(([, ts]) =>
			ts.map((t) => t.disc_total ?? 0).filter((n) => n <= 999)
		);
		const last = Math.max(0, ...declared);
		return last === before ? disc(before) : { key: before + 0.5, label: `After disc ${before}` };
	}
	const next = after ?? discs[0][0];
	return next === 1 ? disc(1) : { key: next - 0.5, label: `Before disc ${next}` };
}

/**
 * The album's tracks, disc by disc as `discs` has them, with rows for the
 * runs of numbers it lacks where they belong (or between discs, where the
 * tags don't say which), and missing discs with no rows.
 */
export function withGaps<T extends Numbered>(
	discs: [number, T[]][],
	inc: Incomplete | undefined
): Group<T>[] {
	const groups = new Map<number, Group<T>>(
		discs.map(([d, tracks]) => [
			d,
			{ key: d, label: `Disc ${d}`, rows: tracks.map((track) => ({ track })) }
		])
	);
	for (const g of inc?.tracks ?? []) {
		const place = g.disc === null ? placeOf(g, discs) : { key: g.disc, label: `Disc ${g.disc}` };
		const group = groups.get(place.key) ?? { ...place, rows: [] };
		const rows = (group.rows ??= []);
		// Before the first track numbered after it.
		const at = rows.findIndex((r) => 'track' in r && (r.track.track_no ?? 0) > g.to);
		rows.splice(at === -1 ? rows.length : at, 0, { from: g.from, to: g.to });
		groups.set(place.key, group);
	}
	for (const d of inc?.discs ?? []) {
		if (!groups.has(d)) groups.set(d, { key: d, label: `Disc ${d}`, rows: null });
	}
	return [...groups.values()].sort((a, b) => a.key - b.key);
}
