// The tag editor's model: every track's value of every tag kind, as the
// album's files have them and as edited, with album-wide fields over them.

/** Track id -> tag kind (catalog id) -> value; '' where the track has none. */
export type Values = Record<number, Record<string, string>>;

/** Album-wide fields shown up front, then the per-track table's columns. */
export const ALBUM_CORE = ['album', 'album_artist', 'date', 'genre'] as const;
export const TRACK_CORE = ['track_number', 'disc_number', 'title', 'artist'] as const;
export const CORE: readonly string[] = [...ALBUM_CORE, ...TRACK_CORE];

/** Kinds whose values run over several lines. */
export const MULTILINE = new Set(['lyrics', 'comment', 'description']);

/** A track's value of `kind`, '' when it has none. */
export const valueOf = (values: Values, id: number, kind: string) => values[id]?.[kind] ?? '';

/** The value every track has for `kind`, or null when they differ. */
export function shared(values: Values, ids: number[], kind: string): string | null {
	const seen = new Set(ids.map((id) => valueOf(values, id, kind)));
	return seen.size <= 1 ? ([...seen][0] ?? '') : null;
}

/**
 * Type `value` into the album-wide field of `kind`: every track gets it.
 * Emptying a field the tracks differed in keeps each track's own value, as
 * an album-wide field left empty does; emptying one they agreed on clears it.
 */
export function setAll(form: Values, base: Values, ids: number[], kind: string, value: string) {
	const keep = value === '' && shared(base, ids, kind) === null;
	for (const id of ids) {
		form[id] ??= {};
		form[id][kind] = keep ? valueOf(base, id, kind) : value;
	}
}

/** Whether track `id`'s `kind` was edited. */
export const changed = (form: Values, base: Values, id: number, kind: string) =>
	valueOf(form, id, kind) !== valueOf(base, id, kind);

/** Every kind any track has, or was given, a value for. */
export function kindsIn(...all: Values[]): Set<string> {
	const out = new Set<string>();
	for (const values of all)
		for (const track of Object.values(values))
			for (const [kind, v] of Object.entries(track)) if (v !== '') out.add(kind);
	return out;
}

/** The edits, by track: each edited kind's new value ('' removes it). */
export function edits(form: Values, base: Values, ids: number[]): Values {
	const out: Values = {};
	for (const id of ids) {
		const kinds = new Set([...Object.keys(form[id] ?? {}), ...Object.keys(base[id] ?? {})]);
		const edited: Record<string, string> = {};
		for (const kind of kinds)
			if (changed(form, base, id, kind)) edited[kind] = valueOf(form, id, kind);
		if (Object.keys(edited).length) out[id] = edited;
	}
	return out;
}

/**
 * How many changes there are: a kind set alike on every track (an
 * album-wide field) counts once, one that differs per track once per track
 * edited.
 */
export function changeCount(form: Values, base: Values, ids: number[]) {
	const byKind = new Map<string, number>();
	for (const track of Object.values(edits(form, base, ids)))
		for (const kind of Object.keys(track)) byKind.set(kind, (byKind.get(kind) ?? 0) + 1);
	let n = 0;
	for (const [kind, tracks] of byKind) n += shared(form, ids, kind) !== null ? 1 : tracks;
	return n;
}

// A part lofty reads whole: two characters (06, or 6 with a space before or
// after it), or one digit with a separator after it; the last part must be
// two, and ends in a space only before a Z (the value is trimmed).
const PART = '(?:\\d{1,2}| \\d|\\d )';
const LAST = '(?:\\d{2}| \\d|\\d (?=Z$))';
const SEPARATED = new RegExp(
	`^\\d{4}(?:[-.](?:${LAST}|${PART}[-.](?:${LAST}|${PART}T(?:${LAST}|${PART}:(?:${LAST}|${PART}:${LAST}))?)))?Z?$`
);
// Run together, a month or day can be a space and a digit (2024 6 3 is
// June 3: lofty reads the spaces as padding, not separators).
const PADDED = '(?:\\d{2}| \\d)';
const COMPACT = new RegExp(
	`^\\d{4}(?:${PADDED}(?:${PADDED}(?:T(?:${LAST}|\\d{2}(?:${LAST}|\\d{2}(?:${LAST})?)?))?)?)?Z?$`
);

/**
 * Numbers and dates the server would refuse; other kinds take any text. A
 * date is one lofty reads whole: a year, then a month, a day and a time
 * after a T, the parts after dashes or dots and the time's after colons
 * (2010.08.25, 2024-6-03T14:8:49), or the digits run together
 * (20100825T1230); a Z may end it. A time after a space, or a last part of
 * one digit (2024-6), would be dropped on writing.
 */
export function invalid(kind: string, value: string): boolean {
	const v = value.trim();
	if (!v) return false;
	if (['track_number', 'track_total', 'disc_number', 'disc_total'].includes(kind))
		return !/^\d+(\/\d+)?$/.test(v);
	if (kind === 'date') return !(SEPARATED.test(v) || COMPACT.test(v));
	return false;
}

/**
 * Edits whose save was refused because the album had changed elsewhere,
 * kept to put on top of the album as it is now.
 */
export interface TagDraft {
	/** The values the edits were made on. */
	base: Values;
	/** The edits, by track (see `edits`). */
	edits: Values;
	/** Where each track's file was: an id freed by a removed track can name a new one. */
	paths: Record<number, string>;
}

/** `values` with a kept draft's edits on top, on the tracks `here` says are still the same. */
export function withKept(values: Values, kept: TagDraft, here: (id: number) => boolean): Values {
	for (const [id, edited] of Object.entries(kept.edits)) {
		if (!here(+id)) continue;
		values[+id] ??= {};
		Object.assign(values[+id], edited);
	}
	return values;
}

/**
 * What changed elsewhere since a kept draft was made: each track's kinds
 * whose value differs from the draft's base, with the value it had then;
 * null for a track that has joined since (or isn't the same one).
 */
export function changedElsewhere(
	kept: TagDraft,
	now: Values,
	here: (id: number) => boolean
): Record<number, Record<string, string | null>> {
	const out: Record<number, Record<string, string | null>> = {};
	for (const [key, values] of Object.entries(now)) {
		const id = +key;
		const old = here(id) ? kept.base[id] : undefined;
		const kinds = new Set([...Object.keys(values), ...Object.keys(old ?? {})]);
		const fields: Record<string, string | null> = {};
		for (const kind of kinds) {
			const was = old ? (old[kind] ?? '') : null;
			if (was !== (values[kind] ?? '')) fields[kind] = was;
		}
		if (Object.keys(fields).length) out[id] = fields;
	}
	return out;
}

/**
 * How the read-only tags look across the album: each key once, with its
 * values (a key can be there several times on one track), shared when every
 * track has the same ones, or else each track's.
 */
export function otherTags(
	tracks: { id: number; other: [string, string][] }[]
): { key: string; values: string[] | null; tracks: { id: number; values: string[] }[] }[] {
	const keys: string[] = [];
	const byKey = new Map<string, Map<number, string[]>>();
	for (const t of tracks) {
		for (const [key, value] of t.other) {
			if (!byKey.has(key)) {
				byKey.set(key, new Map());
				keys.push(key);
			}
			const per = byKey.get(key)!;
			if (!per.has(t.id)) per.set(t.id, []);
			per.get(t.id)!.push(value);
		}
	}
	return keys.map((key) => {
		const per = byKey.get(key)!;
		const rows = [...per].map(([id, values]) => ({ id, values }));
		const same = new Set(rows.map((r) => JSON.stringify(r.values))).size === 1;
		return {
			key,
			values: per.size === tracks.length && same ? rows[0].values : null,
			tracks: rows
		};
	});
}
