// Tag filters: the sidebar narrows the album grid to albums with the picked
// values of its filter tags.

import type { Album } from './api';

/**
 * Picked values by tag kind. An album needs one of the picked values of
 * every kind that has any.
 */
export type Filters = Record<string, string[]>;

/** Values that differ only in case are one value to filter by. */
const key = (v: string) => v.normalize('NFC').toLowerCase();
export const same = (a: string, b: string) => key(a) === key(b);

const values = (a: Album, kind: string) => a.filter_values?.[kind] ?? [];

/** How many values are picked across `kinds`. */
export const picked = (filters: Filters, kinds: string[]) =>
	kinds.reduce((n, k) => n + (filters[k]?.length ?? 0), 0);

export const isPicked = (filters: Filters, kind: string, value: string) =>
	(filters[kind] ?? []).some((v) => same(v, value));

/** `filters` with `value` of `kind` picked, or unpicked if it was. */
export function toggled(filters: Filters, kind: string, value: string): Filters {
	const list = filters[kind] ?? [];
	const next = isPicked(filters, kind, value)
		? list.filter((v) => !same(v, value))
		: [...list, value];
	return { ...filters, [kind]: next };
}

/** A test for albums that pass the filters of `kinds`, leaving out `skip`'s. */
export function matcher(filters: Filters, kinds: string[], skip?: string) {
	const active = kinds
		.filter((k) => k !== skip && filters[k]?.length)
		.map((k) => [k, new Set(filters[k].map(key))] as const);
	return (a: Album) => active.every(([k, keys]) => values(a, k).some((v) => keys.has(key(v))));
}

export interface TagValue {
	value: string;
	/** Albums with it. */
	count: number;
}

const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });

/**
 * The values of `kind` on `albums`, with how many albums have each, labelled
 * by their most common spelling. Years run newest first, everything else A to Z.
 */
export function tagValues(albums: Album[], kind: string): TagValue[] {
	const found = new Map<string, { count: number; spellings: Map<string, number> }>();
	for (const a of albums) {
		const seen = new Set<string>();
		for (const v of values(a, kind)) {
			const k = key(v);
			let entry = found.get(k);
			if (!entry) found.set(k, (entry = { count: 0, spellings: new Map() }));
			entry.spellings.set(v, (entry.spellings.get(v) ?? 0) + 1);
			if (!seen.has(k)) entry.count++;
			seen.add(k);
		}
	}
	const out = [...found.values()].map(({ count, spellings }) => ({
		value: [...spellings].reduce((a, b) => (b[1] > a[1] ? b : a))[0],
		count
	}));
	return out.sort(order(kind));
}

/** How values of `kind` are listed. */
export function order(kind: string) {
	return kind === 'date'
		? (a: TagValue, b: TagValue) => Number(b.value) - Number(a.value)
		: (a: TagValue, b: TagValue) => collator.compare(a.value, b.value);
}
